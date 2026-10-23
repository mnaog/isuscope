//! Goのアプリが負荷の区間で取ったprofile（pprof）を読み、run全体の集計metricにする。
//!
//! アプリの窓口（scaffoldの`profile.go`）は、`go-profile-stop` collectorに
//! `isuscope-go-profile 1 run=<ID> started=<Unix秒> stopped=<Unix秒>`の1行と、
//! `profile <種類> <gzipしたpprofのbase64>`の行を返す。CPUは区間の分だけ、mutex・block・割り当ては
//! 起動からの累計なので`<種類>-base`（startの時点）との差を取る。
//!
//! 出すmetric（時刻なし、run全体）:
//! - `go.cpu.seconds`（sampleのCPU時間の合計）、`go.cpu.window_seconds`（profileを取った区間の長さ）
//! - `go.cpu.flat_seconds`・`go.cpu.cum_seconds`（label `function`）: 関数そのもの／呼んだ先も含めた時間
//! - `go.cpu.app_seconds`（label `function`）: sampleを、最も内側のアプリの関数（`main.`）へ寄せた時間。
//!   標準libraryやruntimeの中で使った時間も、それを呼んだアプリの関数に入る
//! - `go.cpu.class_seconds`（label `class`）: 葉の関数の種類（[`function_class`]）ごと
//! - `go.cpu.route_seconds`（label `method`・`route`）: アプリが要求ごとに付けたlabelごと
//! - `go.mutex.delay_ms`・`go.mutex.contentions`（label `site`）: 取り合いになったmutexを放したアプリの関数
//! - `go.block.delay_ms`・`go.block.count`（label `site`）: goroutineが待ったアプリの関数
//! - `go.alloc.bytes`・`go.alloc.objects`（label `site`）: 割り当てたアプリの関数
//! - `go.goroutines`（label `site`）: stopの時点のgoroutineを、止まっているアプリの関数で数えたもの

use crate::model::Metric;
use anyhow::{Context, Result, bail};
use base64::Engine;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;

/// 関数ごとのmetricは、この数までに絞る（区間の中で多い順）。
const FUNCTION_ROWS: usize = 80;

/// 1つのsampleの呼び出しの連なり（葉が先）と値、label。
struct Sample {
    stack: Vec<String>,
    values: Vec<i64>,
    labels: BTreeMap<String, String>,
}

struct Profile {
    sample_types: Vec<String>,
    samples: Vec<Sample>,
    duration_nanos: i64,
    period: i64,
}

impl Profile {
    fn value_index(&self, name: &str) -> Option<usize> {
        self.sample_types.iter().position(|kind| kind == name)
    }
}

/// collectorの出力全体を読む。
pub fn parse(raw: &str) -> Result<Vec<Metric>> {
    let mut lines = raw.lines().filter(|line| !line.trim().is_empty());
    let header = lines.next().context("go profile output is empty")?;
    let mut fields = header.split_whitespace();
    if fields.next() != Some("isuscope-go-profile") || fields.next() != Some("1") {
        bail!("go profile output does not start with `isuscope-go-profile 1`: {header}");
    }
    let attributes = fields
        .filter_map(|field| field.split_once('='))
        .collect::<BTreeMap<_, _>>();
    let window = match (
        attributes
            .get("started")
            .and_then(|value| value.parse::<f64>().ok()),
        attributes
            .get("stopped")
            .and_then(|value| value.parse::<f64>().ok()),
    ) {
        (Some(started), Some(stopped)) if stopped >= started => Some(stopped - started),
        _ => None,
    };
    let mut profiles = BTreeMap::<String, Profile>::new();
    for line in lines {
        let mut parts = line.splitn(3, ' ');
        let (Some("profile"), Some(kind), Some(data)) = (parts.next(), parts.next(), parts.next())
        else {
            bail!("unexpected go profile line: {}", truncate(line));
        };
        let compressed = base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .with_context(|| format!("profile {kind} is not base64"))?;
        let profile = decode(&compressed).with_context(|| format!("cannot read profile {kind}"))?;
        profiles.insert(kind.to_owned(), profile);
    }
    let mut metrics = Vec::new();
    if let Some(cpu) = profiles.get("cpu") {
        cpu_metrics(cpu, window, &mut metrics);
    }
    for (kind, value_names, names) in [
        (
            "mutex",
            ["delay", "contentions"],
            [
                ("go.mutex.delay_ms", "ms", 1e-6),
                ("go.mutex.contentions", "contentions", 1.0),
            ],
        ),
        (
            "block",
            ["delay", "contentions"],
            [
                ("go.block.delay_ms", "ms", 1e-6),
                ("go.block.count", "events", 1.0),
            ],
        ),
        (
            "allocs",
            ["alloc_space", "alloc_objects"],
            [
                ("go.alloc.bytes", "bytes", 1.0),
                ("go.alloc.objects", "objects", 1.0),
            ],
        ),
    ] {
        let Some(end) = profiles.get(kind) else {
            continue;
        };
        let base = profiles.get(&format!("{kind}-base"));
        site_metrics(end, base, value_names, names, &mut metrics);
    }
    if let Some(goroutines) = profiles.get("goroutine") {
        let mut sites = BTreeMap::<String, f64>::new();
        if let Some(index) = goroutines.value_index("goroutine") {
            for sample in &goroutines.samples {
                *sites.entry(site(&sample.stack)).or_default() +=
                    sample.values.get(index).copied().unwrap_or_default() as f64;
            }
        }
        for (site, count) in top(sites, FUNCTION_ROWS) {
            metrics.push(metric(
                "go.goroutines",
                count,
                "goroutines",
                [("site", site)],
            ));
        }
    }
    Ok(metrics)
}

fn truncate(value: &str) -> String {
    value.chars().take(80).collect()
}

fn metric<const N: usize>(
    name: &str,
    value: f64,
    unit: &str,
    labels: [(&str, String); N],
) -> Metric {
    Metric {
        name: name.into(),
        value: (value * 1e6).round() / 1e6,
        unit: unit.into(),
        timestamp: None,
        labels: labels
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    }
}

fn top(values: BTreeMap<String, f64>, limit: usize) -> Vec<(String, f64)> {
    let mut rows = values
        .into_iter()
        .filter(|(_, value)| *value > 0.0)
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(limit);
    rows
}

fn cpu_metrics(profile: &Profile, window: Option<f64>, metrics: &mut Vec<Metric>) {
    // CPUのprofileの値は[samples/count, cpu/nanoseconds]。時間の方を使う。
    let index = profile.value_index("cpu");
    let seconds = |sample: &Sample| match index {
        Some(index) => sample.values.get(index).copied().unwrap_or_default() as f64 / 1e9,
        None => {
            sample.values.first().copied().unwrap_or_default() as f64 * profile.period as f64 / 1e9
        }
    };
    let mut total = 0.0;
    let mut flat = BTreeMap::<String, f64>::new();
    let mut cumulative = BTreeMap::<String, f64>::new();
    let mut app = BTreeMap::<String, f64>::new();
    let mut classes = BTreeMap::<String, f64>::new();
    let mut routes = BTreeMap::<(String, String), f64>::new();
    for sample in &profile.samples {
        let value = seconds(sample);
        if value <= 0.0 {
            continue;
        }
        total += value;
        let leaf = sample.stack.first().map_or("[unknown]", String::as_str);
        *flat.entry(leaf.to_owned()).or_default() += value;
        *classes.entry(function_class(leaf).to_owned()).or_default() += value;
        let mut seen = std::collections::BTreeSet::new();
        for function in &sample.stack {
            if seen.insert(function.as_str()) {
                *cumulative.entry(function.clone()).or_default() += value;
            }
        }
        let owner = sample
            .stack
            .iter()
            .find(|function| is_app(function))
            .cloned()
            .unwrap_or_else(|| "(outside the app)".into());
        *app.entry(owner).or_default() += value;
        let method = sample.labels.get("method").cloned().unwrap_or_default();
        let route = sample
            .labels
            .get("route")
            .cloned()
            .unwrap_or_else(|| "(no route)".into());
        *routes.entry((method, route)).or_default() += value;
    }
    metrics.push(metric("go.cpu.seconds", total, "seconds", []));
    let window = window.unwrap_or(profile.duration_nanos as f64 / 1e9);
    if window > 0.0 {
        metrics.push(metric("go.cpu.window_seconds", window, "seconds", []));
    }
    for (function, value) in top(flat, FUNCTION_ROWS) {
        metrics.push(metric(
            "go.cpu.flat_seconds",
            value,
            "seconds",
            [("function", function)],
        ));
    }
    for (function, value) in top(cumulative, FUNCTION_ROWS) {
        metrics.push(metric(
            "go.cpu.cum_seconds",
            value,
            "seconds",
            [("function", function)],
        ));
    }
    for (function, value) in top(app, FUNCTION_ROWS) {
        metrics.push(metric(
            "go.cpu.app_seconds",
            value,
            "seconds",
            [("function", function)],
        ));
    }
    for (class, value) in top(classes, usize::MAX) {
        metrics.push(metric(
            "go.cpu.class_seconds",
            value,
            "seconds",
            [("class", class)],
        ));
    }
    let mut routes = routes.into_iter().collect::<Vec<_>>();
    routes.sort_by(|a, b| b.1.total_cmp(&a.1));
    for ((method, route), value) in routes.into_iter().take(FUNCTION_ROWS) {
        metrics.push(metric(
            "go.cpu.route_seconds",
            value,
            "seconds",
            [("method", method), ("route", route)],
        ));
    }
}

/// 累計のprofileの、startからの増えた分をアプリの関数（[`site`]）ごとに足す。
fn site_metrics(
    end: &Profile,
    base: Option<&Profile>,
    value_names: [&str; 2],
    names: [(&str, &str, f64); 2],
    metrics: &mut Vec<Metric>,
) {
    let indexes = value_names.map(|name| end.value_index(name));
    let totals = |profile: &Profile| {
        let mut by_stack = HashMap::<Vec<String>, [f64; 2]>::new();
        let base_indexes = value_names.map(|name| profile.value_index(name));
        for sample in &profile.samples {
            let row = by_stack.entry(sample.stack.clone()).or_default();
            for (slot, index) in base_indexes.iter().enumerate() {
                if let Some(index) = index {
                    row[slot] += sample.values.get(*index).copied().unwrap_or_default() as f64;
                }
            }
        }
        by_stack
    };
    if indexes.iter().all(Option::is_none) {
        return;
    }
    let before = base.map(totals).unwrap_or_default();
    let mut sites = [BTreeMap::<String, f64>::new(), BTreeMap::new()];
    for (stack, values) in totals(end) {
        let previous = before.get(&stack).copied().unwrap_or_default();
        let owner = site(&stack);
        for slot in 0..2 {
            let delta = values[slot] - previous[slot];
            if delta > 0.0 {
                *sites[slot].entry(owner.clone()).or_default() += delta;
            }
        }
    }
    for (slot, values) in sites.into_iter().enumerate() {
        let (name, unit, scale) = names[slot];
        for (site, value) in top(values, FUNCTION_ROWS) {
            metrics.push(metric(name, value * scale, unit, [("site", site)]));
        }
    }
}

fn is_app(function: &str) -> bool {
    function.starts_with("main.")
}

/// 呼び出しの連なりから、直す場所として示す関数。最も内側のアプリの関数（`main.`）、無ければ
/// runtime・sync・internalでない最も内側の関数、それも無ければ葉の関数。
pub fn site(stack: &[String]) -> String {
    if let Some(function) = stack.iter().find(|function| is_app(function)) {
        return function.clone();
    }
    stack
        .iter()
        .find(|function| {
            !(function.starts_with("runtime.")
                || function.starts_with("runtime/")
                || function.starts_with("sync.")
                || function.starts_with("sync/")
                || function.starts_with("internal/"))
        })
        .or_else(|| stack.first())
        .cloned()
        .unwrap_or_else(|| "[unknown]".into())
}

/// Goの関数名（perfのsymbolも同じ形）を、CPUを何に使ったかの種類に分ける。Goの関数名はpackageの
/// pathで始まるので、名前だけで分けられる。
pub fn function_class(function: &str) -> &'static str {
    let name = function.trim();
    let starts = |prefixes: &[&str]| prefixes.iter().any(|prefix| name.starts_with(prefix));
    if name.is_empty() || name == "[unknown]" {
        return "unknown";
    }
    if starts(&["main."]) {
        return "app";
    }
    if starts(&[
        "runtime/pprof.",
        "runtime/trace.",
        "runtime.sigprof",
        "runtime.traceAdvance",
        "runtime.traceReader",
        "runtime.(*traceWriter)",
        "runtime.(*profBuf)",
    ]) {
        return "profiling";
    }
    if starts(&[
        "runtime.gcBgMarkWorker",
        "runtime.gcDrain",
        "runtime.gcMark",
        "runtime.gcStart",
        "runtime.gcSweep",
        "runtime.gcAssist",
        "runtime.gcFlushBgCredit",
        "runtime.gcWriteBarrier",
        "runtime.wbBuf",
        "runtime.scanobject",
        "runtime.scanblock",
        "runtime.scanstack",
        "runtime.scanframe",
        "runtime.scanSpan",
        "runtime.scanConservative",
        "runtime.greyobject",
        "runtime.markroot",
        "runtime.markBits",
        "runtime.findObject",
        "runtime.heapBits",
        "runtime.heapSetType",
        "runtime.typePointers",
        "runtime.(*gcWork)",
        "runtime.(*gcBits)",
        "runtime.(*mspan).sweep",
        "runtime.(*sweepLocked)",
        "runtime.(*spanInlineMarkBits)",
        "runtime.(*spanQueue)",
        "runtime.tryDeferToSpanScan",
        "runtime.tryGetSpan",
        "runtime.bgsweep",
        "runtime.sweepone",
        "runtime.bgscavenge",
        "runtime.(*pageAlloc).scavenge",
        "runtime.(*scavengerState)",
        "runtime.wbMove",
        "runtime.bulkBarrierPreWrite",
        "runtime.typedmemmove",
    ]) {
        return "gc";
    }
    if starts(&[
        "runtime.mallocgc",
        "runtime.newobject",
        "runtime.newarray",
        "runtime.makeslice",
        "runtime.growslice",
        "runtime.makemap",
        "runtime.rawstring",
        "runtime.rawbyteslice",
        "runtime.concatstring",
        "runtime.slicebytetostring",
        "runtime.stringtoslicebyte",
        "runtime.nextFreeFast",
        "runtime.(*mcache)",
        "runtime.(*mcentral)",
        "runtime.(*mheap)",
        "runtime.(*mspan).nextFreeIndex",
        "runtime.(*mspan).init",
        "runtime.memclrNoHeapPointers",
        "runtime.deductAssistCredit",
        "runtime.convT",
        "runtime.convTstring",
        "runtime.convTslice",
        "runtime.mProf_Malloc",
        "runtime.profilealloc",
    ]) {
        return "alloc";
    }
    if starts(&[
        "runtime.lock",
        "runtime.unlock",
        "runtime.semacquire",
        "runtime.semrelease",
        "runtime.(*semaRoot)",
        "runtime.procyield",
        "runtime.sync_runtime_Sem",
        "runtime.sync_runtime_canSpin",
        "sync.",
        "internal/sync.",
    ]) {
        return "lock";
    }
    if starts(&[
        "runtime.chansend",
        "runtime.chanrecv",
        "runtime.selectgo",
        "runtime.selectnbrecv",
        "runtime.selectnbsend",
        "runtime.closechan",
        "runtime.send",
        "runtime.recv",
    ]) {
        return "channel";
    }
    if starts(&[
        "runtime.schedule",
        "runtime.findRunnable",
        "runtime.findrunnable",
        "runtime.park_m",
        "runtime.mcall",
        "runtime.gopark",
        "runtime.goready",
        "runtime.ready",
        "runtime.runqgrab",
        "runtime.runqsteal",
        "runtime.runqput",
        "runtime.stealWork",
        "runtime.netpoll",
        "runtime.futex",
        "runtime.notesleep",
        "runtime.notewakeup",
        "runtime.notetsleep",
        "runtime.stopm",
        "runtime.startm",
        "runtime.wakep",
        "runtime.usleep",
        "runtime.osyield",
        "runtime.mPark",
        "runtime.semasleep",
        "runtime.semawakeup",
        "runtime.kevent",
        "runtime.epollwait",
        "runtime.pthread_cond",
        "runtime.casgstatus",
        "runtime.execute",
        "runtime.gogo",
        "runtime.goexit",
        "runtime.newproc",
        "runtime.gfget",
        "runtime.checkTimers",
        "runtime.(*timers)",
        "runtime.(*timer)",
        "runtime.resetForSleep",
        "runtime.injectglist",
        "runtime.handoffp",
        "runtime.acquirep",
        "runtime.releasep",
        "runtime.entersyscall",
        "runtime.exitsyscall",
        "runtime.reentersyscall",
        "runtime.sysmon",
        "runtime.retake",
        "runtime.preemptone",
        "runtime.asyncPreempt",
        "runtime.morestack",
        "runtime.newstack",
        "runtime.copystack",
        "runtime.mstart",
        "runtime.systemstack",
        "runtime.nanotime",
        "runtime.walltime",
        "runtime.madvise",
    ]) {
        return "scheduler";
    }
    if starts(&[
        "syscall.",
        "internal/runtime/syscall",
        "runtime/internal/syscall",
        "internal/poll.",
        "internal/syscall",
        "os.",
    ]) || matches!(
        name,
        "runtime.read" | "runtime.write" | "runtime.write1" | "runtime.write2"
    ) {
        return "syscall";
    }
    if starts(&[
        "net/http.",
        "net/textproto.",
        "golang.org/x/net/http",
        "vendor/golang.org/x/net/http",
    ]) {
        return "http";
    }
    if starts(&["net.", "net/", "bufio."]) {
        return "net";
    }
    if starts(&[
        "crypto/",
        "vendor/golang.org/x/crypto",
        "golang.org/x/crypto",
    ]) {
        return "crypto";
    }
    if starts(&[
        "encoding/json",
        "github.com/goccy/go-json",
        "github.com/bytedance/sonic",
        "github.com/json-iterator",
        "github.com/mailru/easyjson",
    ]) {
        return "json";
    }
    if starts(&[
        "database/sql",
        "github.com/go-sql-driver/mysql",
        "github.com/jmoiron/sqlx",
        "github.com/jackc/",
        "github.com/lib/pq",
    ]) {
        return "sql";
    }
    if starts(&["log.", "log/"]) {
        return "log";
    }
    if starts(&[
        "runtime.",
        "runtime/",
        "internal/",
        "indexbyte",
        "memeqbody",
        "cmpbody",
        "countbody",
    ]) {
        return "runtime";
    }
    if !name.contains('.') || name.starts_with('[') {
        // 区切りの無い名前はGoの関数ではない（Cの関数、kernel、名前の無いsymbol）。
        return "other";
    }
    // 外のmoduleはpathの最初の区切りがhost名（`github.com/...`）。標準libraryには`.`が無い。
    match name.split_once('/') {
        Some((host, _)) if host.contains('.') => "library",
        _ => "stdlib",
    }
}

// ---- pprofのprotobuf（profile.proto）の最小限の読み取り ----

fn decode(compressed: &[u8]) -> Result<Profile> {
    let mut bytes = Vec::new();
    if compressed.starts_with(&[0x1f, 0x8b]) {
        flate2::read::GzDecoder::new(compressed)
            .read_to_end(&mut bytes)
            .context("the profile is not valid gzip")?;
    } else {
        bytes.extend_from_slice(compressed);
    }
    let mut strings = Vec::<String>::new();
    let mut sample_types = Vec::<(i64, i64)>::new();
    let mut raw_samples = Vec::<(Vec<u64>, Vec<i64>, Vec<(i64, i64)>)>::new();
    let mut locations = HashMap::<u64, Vec<u64>>::new();
    let mut functions = HashMap::<u64, i64>::new();
    let mut duration_nanos = 0;
    let mut period = 0;
    let mut reader = Reader::new(&bytes);
    while let Some((field, value)) = reader.field()? {
        match (field, value) {
            (1, Value::Bytes(data)) => {
                let mut kind = (0, 0);
                let mut inner = Reader::new(data);
                while let Some((field, value)) = inner.field()? {
                    match (field, value) {
                        (1, Value::Varint(value)) => kind.0 = value as i64,
                        (2, Value::Varint(value)) => kind.1 = value as i64,
                        _ => {}
                    }
                }
                sample_types.push(kind);
            }
            (2, Value::Bytes(data)) => {
                let mut locations = Vec::new();
                let mut values = Vec::new();
                let mut labels = Vec::new();
                let mut inner = Reader::new(data);
                while let Some((field, value)) = inner.field()? {
                    match (field, value) {
                        (1, value) => value.unsigned_into(&mut locations)?,
                        (2, value) => {
                            let mut unsigned = Vec::new();
                            value.unsigned_into(&mut unsigned)?;
                            values.extend(unsigned.into_iter().map(|value| value as i64));
                        }
                        (3, Value::Bytes(data)) => {
                            let mut label = (0, 0);
                            let mut fields = Reader::new(data);
                            while let Some((field, value)) = fields.field()? {
                                match (field, value) {
                                    (1, Value::Varint(value)) => label.0 = value as i64,
                                    (2, Value::Varint(value)) => label.1 = value as i64,
                                    _ => {}
                                }
                            }
                            labels.push(label);
                        }
                        _ => {}
                    }
                }
                raw_samples.push((locations, values, labels));
            }
            (4, Value::Bytes(data)) => {
                let mut id = 0;
                let mut lines = Vec::new();
                let mut inner = Reader::new(data);
                while let Some((field, value)) = inner.field()? {
                    match (field, value) {
                        (1, Value::Varint(value)) => id = value,
                        (4, Value::Bytes(line)) => {
                            let mut fields = Reader::new(line);
                            while let Some((field, value)) = fields.field()? {
                                if let (1, Value::Varint(function)) = (field, value) {
                                    lines.push(function);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                locations.insert(id, lines);
            }
            (5, Value::Bytes(data)) => {
                let mut id = 0;
                let mut name = 0;
                let mut inner = Reader::new(data);
                while let Some((field, value)) = inner.field()? {
                    match (field, value) {
                        (1, Value::Varint(value)) => id = value,
                        (2, Value::Varint(value)) => name = value as i64,
                        _ => {}
                    }
                }
                functions.insert(id, name);
            }
            (6, Value::Bytes(data)) => strings.push(String::from_utf8_lossy(data).into_owned()),
            (10, Value::Varint(value)) => duration_nanos = value as i64,
            (12, Value::Varint(value)) => period = value as i64,
            _ => {}
        }
    }
    let text = |index: i64| {
        usize::try_from(index)
            .ok()
            .and_then(|index| strings.get(index))
            .cloned()
            .unwrap_or_default()
    };
    let samples = raw_samples
        .into_iter()
        .map(|(location_ids, values, labels)| {
            // locationの中の行は、内側（inlineされた関数）が先、呼び出し元が後。sampleのlocationは葉が先。
            let stack = location_ids
                .iter()
                .flat_map(|id| locations.get(id).cloned().unwrap_or_default())
                .map(|function| text(functions.get(&function).copied().unwrap_or_default()))
                .collect();
            Sample {
                stack,
                values,
                labels: labels
                    .into_iter()
                    .filter(|(_, value)| *value != 0)
                    .map(|(key, value)| (text(key), text(value)))
                    .collect(),
            }
        })
        .collect();
    Ok(Profile {
        sample_types: sample_types
            .into_iter()
            .map(|(kind, _)| text(kind))
            .collect(),
        samples,
        duration_nanos,
        period,
    })
}

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

impl Value<'_> {
    /// repeatedの数（packedでも1つずつでも）をoutへ足す。
    fn unsigned_into(self, out: &mut Vec<u64>) -> Result<()> {
        match self {
            Value::Varint(value) => out.push(value),
            Value::Bytes(data) => {
                let mut reader = Reader::new(data);
                while !reader.done() {
                    out.push(reader.varint()?);
                }
            }
            Value::Fixed => {}
        }
        Ok(())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn done(&self) -> bool {
        self.position >= self.bytes.len()
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let Some(&byte) = self.bytes.get(self.position) else {
                bail!("the profile ends inside a number");
            };
            self.position += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        bail!("a number in the profile is too long")
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .context("the profile ends inside a field")?;
        let data = &self.bytes[self.position..end];
        self.position = end;
        Ok(data)
    }

    fn field(&mut self) -> Result<Option<(u64, Value<'a>)>> {
        if self.done() {
            return Ok(None);
        }
        let key = self.varint()?;
        let value = match key & 7 {
            0 => Value::Varint(self.varint()?),
            1 => {
                self.take(8)?;
                Value::Fixed
            }
            2 => {
                let length = usize::try_from(self.varint()?)?;
                Value::Bytes(self.take(length)?)
            }
            5 => {
                self.take(4)?;
                Value::Fixed
            }
            other => bail!("unsupported protobuf wire type {other} in the profile"),
        };
        Ok(Some((key >> 3, value)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/go-profile-stop.txt");

    fn value(metrics: &[Metric], name: &str, label: (&str, &str)) -> Option<f64> {
        metrics
            .iter()
            .find(|metric| {
                metric.name == name
                    && metric.labels.get(label.0).map(String::as_str) == Some(label.1)
            })
            .map(|metric| metric.value)
    }

    #[test]
    fn a_real_stop_output_becomes_cpu_route_mutex_and_alloc_metrics() {
        let metrics = parse(FIXTURE).unwrap();
        let total = metrics
            .iter()
            .find(|metric| metric.name == "go.cpu.seconds")
            .unwrap()
            .value;
        // `go tool pprof -top`の`Total samples = 6.31s`と同じ。
        assert!((total - 6.31).abs() < 0.005, "{total}");
        let sha = value(
            &metrics,
            "go.cpu.flat_seconds",
            ("function", "crypto/internal/fips140/sha256.blockGeneric"),
        )
        .unwrap();
        assert!((sha - 3.31).abs() < 0.005, "{sha}");
        // `go tool pprof -tags`の`/api/rides/:id 1.94s`と`/api/rides/:key 1.91s`。
        let id = value(
            &metrics,
            "go.cpu.route_seconds",
            ("route", "/api/rides/:id"),
        )
        .unwrap();
        let key = value(
            &metrics,
            "go.cpu.route_seconds",
            ("route", "/api/rides/:key"),
        )
        .unwrap();
        assert!(
            (id - 1.94).abs() < 0.005 && (key - 1.91).abs() < 0.005,
            "{id} {key}"
        );
        assert_eq!(
            value(&metrics, "go.cpu.class_seconds", ("class", "crypto")).map(|v| v > 3.0),
            Some(true)
        );
        // アプリの関数へ寄せると、sha256を呼んだhandlerに入る。
        assert!(
            value(
                &metrics,
                "go.cpu.app_seconds",
                ("function", "main.main.func1")
            )
            .unwrap()
                > 3.4
        );
        // mutexの取り合いは、放したアプリの関数に付く（`-base`との差は5.91ms）。
        let delay = metrics
            .iter()
            .filter(|metric| metric.name == "go.mutex.delay_ms")
            .map(|metric| metric.value)
            .sum::<f64>();
        assert!((delay - 5.91).abs() < 0.02, "{delay}");
        assert!(value(&metrics, "go.mutex.delay_ms", ("site", "main.main.func1")).unwrap() > 4.7);
        assert!(metrics.iter().any(|metric| metric.name == "go.alloc.bytes"));
        assert!(
            metrics
                .iter()
                .any(|metric| metric.name == "go.block.delay_ms")
        );
        assert!(metrics.iter().any(|metric| metric.name == "go.goroutines"));
    }

    #[test]
    fn broken_output_is_an_error() {
        assert!(parse("").is_err());
        assert!(parse("something else\n").is_err());
        assert!(parse("isuscope-go-profile 1 run=a\nprofile cpu !!!\n").is_err());
        assert!(parse("isuscope-go-profile 1 run=a\nprofile cpu AAAA\n").is_err());
        // profileの行が無ければmetricも無い。
        assert!(
            parse("isuscope-go-profile 1 run=a started=1 stopped=2\n")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn function_names_fall_into_classes() {
        for (function, class) in [
            ("main.(*memStore).matchAll", "app"),
            ("runtime.mallocgcSmallScanNoHeader", "alloc"),
            ("runtime.scanobject", "gc"),
            ("runtime.tryDeferToSpanScan", "gc"),
            ("runtime.lock2", "lock"),
            ("sync.(*RWMutex).RLock", "lock"),
            ("runtime.selectgo", "channel"),
            ("runtime.findRunnable", "scheduler"),
            ("internal/runtime/syscall/linux.Syscall6", "syscall"),
            ("runtime.write1", "syscall"),
            ("runtime.readgstatus", "runtime"),
            ("net/textproto.readMIMEHeader", "http"),
            ("net/http.(*conn).readRequest", "http"),
            ("crypto/internal/fips140/aes/gcm.gcmAesEnc", "crypto"),
            ("encoding/json/v2.makeStructArshaler.func2", "json"),
            (
                "github.com/go-sql-driver/mysql.(*mysqlConn).readPacket",
                "sql",
            ),
            ("github.com/go-chi/chi/v5.(*Mux).routeHTTP", "library"),
            ("strconv.AppendFloat", "stdlib"),
            ("runtime.mapaccess2_faststr", "runtime"),
            ("internal/runtime/maps.memHashAES", "runtime"),
            ("indexbytebody", "runtime"),
            ("runtime/pprof.(*profMap).lookup", "profiling"),
            ("pv_native_safe_halt", "other"),
            ("[unknown]", "unknown"),
        ] {
            assert_eq!(function_class(function), class, "{function}");
        }
    }

    #[test]
    fn the_site_is_the_innermost_app_function() {
        let stack = |names: &[&str]| {
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            site(&stack(&[
                "sync.(*Mutex).Unlock",
                "main.(*store).put",
                "main.handler"
            ])),
            "main.(*store).put"
        );
        assert_eq!(
            site(&stack(&[
                "runtime.chanrecv",
                "net/http.(*conn).serve",
                "runtime.goexit"
            ])),
            "net/http.(*conn).serve"
        );
        assert_eq!(
            site(&stack(&["runtime.gcBgMarkWorker"])),
            "runtime.gcBgMarkWorker"
        );
        assert_eq!(site(&[]), "[unknown]");
    }
}
