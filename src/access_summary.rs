//! access logの差分を、node上で1回読んで集計する（`isuscope-node access-summary`）。
//!
//! 以前のalp collectorは、awkで差分を5秒ごとのfileに分け、upstreamと接続の値をsortで並べ、
//! 差分全体とfileごとにalpを走らせていた。2コアのnodeで62万行に約20秒かかり、行数に比例して
//! 伸びた（practice-14-seventhで322万行・87秒）。ここは1回の読み取りで同じ形の出力を作る。
//!
//! 出力は`alp-windows` parserが読む形のまま：接続とupstreamのmetricの行、`whole\t<表>`、
//! `lines\t<数>`、`<bucketの先頭>\t<表>`。表はalpの`--format json`と同じ並び（見出しの行と、
//! method・routeごとの行）で、時間は秒。
//!
//! 行の読み方
//! - tabで分け、最初の`:`までをkey、残りを値にする（同じkeyは後のもの）。空白だけの行は数えない。
//! - methodとuriがある行を数える（`lines`）。そのうち`reqtime`が数の行を表に入れる。入らない行は
//!   parserが`http.access_log_skipped`に数える。
//! - routeはuriの`?`より前。`--matching-groups`のregex（`,`区切り）に最初に一致した行は、そのregexの
//!   文字列をrouteにする（parserがroutes.tomlの名前へ直す）。
//! - statusは整数なら1xx〜5xxに数え、sizeは数なら合計する。分位点は最近接順位（n×pを切り上げた順位）。
//! - 1つの表のrouteが[`ROUTE_LIMIT`]種類を超えたら、残りは`/__too_many_routes__`にまとめてstderrに書く。
//! - `$msec`がベンチの区間[始まり, 終わり)に入る行だけを、5秒bucket（負荷の始まり、分からなければ
//!   ベンチの始まりから区切る。始まりをまたぐbucketは始まりを先頭にする）、接続、upstreamへ回す。
//!
//! nodeへ置く`isuscope-node`はこのfileを取り込んでbuildする。手元のisuscopeとnodeのbinaryが
//! 同じ集計をするよう、このfileの内容から作った[`BUILD_ID`]をcollectorのcommandで照合する。

use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};

/// このfileの内容のFNV-1a（64 bit）。nodeのbinaryが手元と同じ集計かを照合する。
pub const BUILD_ID: &str =
    match core::str::from_utf8(&hex(fnv1a(include_bytes!("access_summary.rs")))) {
        Ok(id) => id,
        Err(_) => panic!("build id is not hex"),
    };

const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    hash
}

const fn hex(value: u64) -> [u8; 16] {
    let digits = b"0123456789abcdef";
    let mut out = [0u8; 16];
    let mut index = 0;
    while index < 16 {
        out[index] = digits[((value >> ((15 - index) * 4)) & 0xf) as usize];
        index += 1;
    }
    out
}

/// 1つの表に持つrouteの上限。routes.tomlが無いと、IDを含むURIが1つずつrouteになる。
pub const ROUTE_LIMIT: usize = 5000;
const OVERFLOW_ROUTE: &str = "/__too_many_routes__";

const HEADER: &str = r#"["count","1xx","2xx","3xx","4xx","5xx","method","uri","min","max","sum","avg","p50","p95","p99","sum_body"]"#;

/// `isuscope-node`と`isuscope __access-summary`の入口。終了コードを返す。
pub fn cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("--build-id") => {
            println!("{BUILD_ID}");
            0
        }
        Some("access-summary") => match Options::parse(&args[1..]) {
            Ok((input, options)) => {
                let stdout = std::io::stdout();
                let mut out = std::io::BufWriter::new(stdout.lock());
                let code = match summarize(&input, &options, &mut out) {
                    Ok(code) => code,
                    Err(error) => {
                        eprintln!("{error}");
                        1
                    }
                };
                if out.flush().is_err() {
                    return 1;
                }
                code
            }
            Err(error) => {
                eprintln!("{error}");
                2
            }
        },
        _ => {
            eprintln!(
                "usage: isuscope-node --build-id | access-summary --input FILE \
                 [--begin EPOCH] [--load-start EPOCH] [--end EPOCH] [--matching-groups CSV]"
            );
            2
        }
    }
}

/// ベンチの区間（空なら分からない）とroutesのregex（`,`区切り）。
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub begin: Option<f64>,
    pub load_start: Option<f64>,
    pub end: Option<f64>,
    pub matching_groups: String,
}

impl Options {
    fn parse(args: &[String]) -> Result<(String, Options), String> {
        let mut input = None;
        let mut options = Options::default();
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
            let epoch = |value: &str| -> Result<Option<f64>, String> {
                if value.is_empty() {
                    return Ok(None);
                }
                value
                    .parse::<f64>()
                    .map(Some)
                    .map_err(|_| format!("{flag} must be an epoch second: {value}"))
            };
            match flag.as_str() {
                "--input" => input = Some(value.clone()),
                "--begin" => options.begin = epoch(value)?,
                "--load-start" => options.load_start = epoch(value)?,
                "--end" => options.end = epoch(value)?,
                "--matching-groups" => options.matching_groups = value.clone(),
                _ => return Err(format!("unknown argument {flag}")),
            }
        }
        Ok((input.ok_or("--input is required")?, options))
    }

    /// 5秒bucketの起点。負荷の始まり、分からなければベンチの始まり。
    fn origin(&self) -> f64 {
        self.load_start.or(self.begin).unwrap_or(0.0)
    }

    fn bucket_end(&self, at: f64) -> f64 {
        let origin = self.origin();
        origin + (((at - origin) / 5.0).floor() + 1.0) * 5.0
    }

    /// bucketの先頭。ベンチの始まりをまたぐbucketは、始まりを先頭にする。
    fn bucket_start(&self, at: f64) -> f64 {
        let start = self.bucket_end(at) - 5.0;
        match self.begin {
            Some(begin) if at >= begin && start < begin => begin,
            _ => start,
        }
    }

    fn in_window(&self, at: f64) -> bool {
        self.begin.is_none_or(|begin| at >= begin) && self.end.is_none_or(|end| at < end)
    }
}

/// 差分のfileを集計してoutへ書く。行はあるのにmethodとuriを1件も読めなければ65を返す。
pub fn summarize(input: &str, options: &Options, out: &mut impl Write) -> Result<i32, String> {
    let groups = compile_groups(&options.matching_groups)?;
    let file = File::open(input).map_err(|error| format!("cannot read {input}: {error}"))?;
    let mut summary = Summary::new(options, &groups);
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line = Vec::with_capacity(1024);
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("cannot read {input}: {error}"))?;
        if read == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        summary.add(&line);
    }
    let io = |error: std::io::Error| error.to_string();
    summary.write_connections(out).map_err(io)?;
    if summary.lines > 0 && summary.parsed == 0 {
        eprintln!(
            "none of {} access-log lines had the method and uri fields",
            summary.lines
        );
        return Ok(65);
    }
    summary.write_samples(out).map_err(io)?;
    for (name, table) in std::iter::once(("whole", &summary.whole)).chain(
        summary
            .buckets
            .iter()
            .map(|(name, table)| (name.as_str(), table)),
    ) {
        if table.overflowed {
            eprintln!(
                "more than {ROUTE_LIMIT} routes in {name}; the rest are counted as {OVERFLOW_ROUTE} (group them in .isuscope/routes.toml)"
            );
        }
    }
    writeln!(out, "whole\t{}", summary.whole.json()).map_err(io)?;
    writeln!(out, "lines\t{}", summary.parsed).map_err(io)?;
    for (bucket, table) in &summary.buckets {
        writeln!(out, "{bucket}\t{}", table.json()).map_err(io)?;
    }
    Ok(0)
}

fn compile_groups(csv: &str) -> Result<Vec<(String, Regex)>, String> {
    csv.split(',')
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .map(|source| {
            Regex::new(source)
                .map(|regex| (source.to_owned(), regex))
                .map_err(|error| format!("invalid matching group {source}: {error}"))
        })
        .collect()
}

/// 1行の、集計に使う値。無いkeyは`None`。
#[derive(Default)]
struct Fields<'a> {
    method: Option<&'a [u8]>,
    uri: Option<&'a [u8]>,
    status: Option<&'a [u8]>,
    size: Option<&'a [u8]>,
    reqtime: Option<&'a [u8]>,
    msec: Option<&'a [u8]>,
    conn: Option<&'a [u8]>,
    upstream_addr: Option<&'a [u8]>,
    upstream_connect: Option<&'a [u8]>,
    upstream_header: Option<&'a [u8]>,
    apptime: Option<&'a [u8]>,
}

impl<'a> Fields<'a> {
    fn parse(line: &'a [u8]) -> Self {
        let mut fields = Self::default();
        for field in line.split(|byte| *byte == b'\t') {
            let Some(colon) = field.iter().position(|byte| *byte == b':') else {
                continue;
            };
            let value = Some(&field[colon + 1..]);
            match &field[..colon] {
                b"method" => fields.method = value,
                b"uri" => fields.uri = value,
                b"status" => fields.status = value,
                b"size" => fields.size = value,
                b"reqtime" => fields.reqtime = value,
                b"msec" => fields.msec = value,
                b"conn" => fields.conn = value,
                b"upstream_addr" => fields.upstream_addr = value,
                b"upstream_connect" => fields.upstream_connect = value,
                b"upstream_header" => fields.upstream_header = value,
                b"apptime" => fields.apptime = value,
                _ => {}
            }
        }
        fields
    }
}

/// 有限の数として読める値（nginxの`-`や空は`None`）。
fn number(value: &[u8]) -> Option<f64> {
    std::str::from_utf8(value)
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

fn duration(value: &[u8]) -> Option<f64> {
    number(value).filter(|seconds| *seconds >= 0.0)
}

struct Route {
    method: String,
    uri: String,
    count: u64,
    statuses: [u64; 5],
    min: f64,
    max: f64,
    sum: f64,
    times: Vec<f64>,
    body: f64,
}

/// method・routeごとの集計（alpの`--format json`の表と同じ並びで出す）。
#[derive(Default)]
struct Table {
    index: HashMap<Vec<u8>, usize>,
    routes: Vec<Route>,
    overflowed: bool,
}

impl Table {
    fn insert(&mut self, method: &[u8], route: &str, time: f64, status: Option<u16>, size: f64) {
        let mut key = Vec::with_capacity(method.len() + 1 + route.len());
        key.extend_from_slice(method);
        key.push(0);
        key.extend_from_slice(route.as_bytes());
        let index = match self.index.get(&key) {
            Some(&index) => index,
            None if self.routes.len() >= ROUTE_LIMIT && route != OVERFLOW_ROUTE => {
                self.overflowed = true;
                return self.insert(method, OVERFLOW_ROUTE, time, status, size);
            }
            None => {
                self.index.insert(key, self.routes.len());
                self.routes.push(Route {
                    method: String::from_utf8_lossy(method).into_owned(),
                    uri: route.to_owned(),
                    count: 0,
                    statuses: [0; 5],
                    min: f64::INFINITY,
                    max: f64::NEG_INFINITY,
                    sum: 0.0,
                    times: Vec::new(),
                    body: 0.0,
                });
                self.routes.len() - 1
            }
        };
        let route = &mut self.routes[index];
        route.count += 1;
        if let Some(status @ 100..=599) = status {
            route.statuses[usize::from(status / 100 - 1)] += 1;
        }
        route.min = route.min.min(time);
        route.max = route.max.max(time);
        route.sum += time;
        route.times.push(time);
        route.body += size;
    }

    fn json(&self) -> String {
        let mut order = (0..self.routes.len()).collect::<Vec<_>>();
        order.sort_by_key(|&index| self.routes[index].count);
        let mut text = String::with_capacity(64 + self.routes.len() * 160);
        text.push('[');
        text.push_str(HEADER);
        for index in order {
            let route = &self.routes[index];
            let mut times = route.times.clone();
            times.sort_by(f64::total_cmp);
            let _ = write!(text, ",[{}", route.count);
            for count in route.statuses {
                let _ = write!(text, ",{count}");
            }
            text.push(',');
            json_string(&mut text, &route.method);
            text.push(',');
            json_string(&mut text, &route.uri);
            for value in [
                route.min,
                route.max,
                route.sum,
                route.sum / route.count as f64,
                nearest_rank(&times, 50),
                nearest_rank(&times, 95),
                nearest_rank(&times, 99),
                route.body,
            ] {
                let _ = write!(text, ",{value}");
            }
            text.push(']');
        }
        text.push(']');
        text
    }
}

/// 並べた値の最近接順位の分位点（n×p/100を切り上げた順位）。
fn nearest_rank(sorted: &[f64], percent: usize) -> f64 {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn json_string(text: &mut String, value: &str) {
    text.push('"');
    for character in value.chars() {
        match character {
            '"' => text.push_str("\\\""),
            '\\' => text.push_str("\\\\"),
            character if (character as u32) < 0x20 => {
                let _ = write!(text, "\\u{:04x}", character as u32);
            }
            character => text.push(character),
        }
    }
    text.push('"');
}

/// `upstream_addr`などの試行の並び（retryは`, `、別のupstreamへ回ったときは` : `で区切る）。
/// 分けた各部分のvalueの中の範囲をoutへ入れる。空なら0個。
fn attempts(value: &[u8], out: &mut Vec<(usize, usize)>) {
    out.clear();
    if value.is_empty() {
        return;
    }
    let mut push = |start: usize, end: usize| {
        let part = &value[start..end];
        let leading = part.iter().take_while(|byte| **byte == b' ').count();
        let trailing = part[leading..]
            .iter()
            .rev()
            .take_while(|byte| **byte == b' ')
            .count();
        out.push((start + leading, end - trailing));
    };
    let (mut start, mut index) = (0, 0);
    while index < value.len() {
        if value[index] == b',' {
            push(start, index);
            start = index + 1;
            index += 1;
        } else if value[index..].starts_with(b" : ") {
            push(start, index);
            start = index + 3;
            index += 3;
        } else {
            index += 1;
        }
    }
    push(start, value.len());
}

struct Connection {
    first: f64,
    last: f64,
    requests: u64,
}

#[derive(Default)]
struct Upstream {
    requests: u64,
    retried: Option<u64>,
}

struct Summary<'a> {
    options: &'a Options,
    groups: &'a [(String, Regex)],
    lines: u64,
    parsed: u64,
    whole: Table,
    buckets: BTreeMap<String, Table>,
    connection_index: HashMap<Vec<u8>, usize>,
    connections: Vec<Connection>,
    opened: BTreeMap<String, u64>,
    upstream_index: HashMap<Vec<u8>, usize>,
    upstreams: Vec<(Vec<u8>, Upstream)>,
    /// key（種類の1文字と、bucketかupstream）ごとの値（ms）。
    samples: HashMap<Vec<u8>, Vec<f64>>,
    /// 同じbucketが続くので、直前のbucketの名前を使い回す。
    last_bucket: Option<(f64, String)>,
    key: Vec<u8>,
    /// upstreamの試行（addr、connect、header、response）を分けた範囲。
    attempts: [Vec<(usize, usize)>; 4],
}

impl<'a> Summary<'a> {
    fn new(options: &'a Options, groups: &'a [(String, Regex)]) -> Self {
        Self {
            options,
            groups,
            lines: 0,
            parsed: 0,
            whole: Table::default(),
            buckets: BTreeMap::new(),
            connection_index: HashMap::new(),
            connections: Vec::new(),
            opened: BTreeMap::new(),
            upstream_index: HashMap::new(),
            upstreams: Vec::new(),
            samples: HashMap::new(),
            last_bucket: None,
            key: Vec::new(),
            attempts: Default::default(),
        }
    }

    fn sample(&mut self, kind: u8, rest: &[u8], value: f64) {
        self.key.clear();
        self.key.push(kind);
        self.key.extend_from_slice(rest);
        match self.samples.get_mut(&self.key[..]) {
            Some(values) => values.push(value),
            None => {
                self.samples.insert(self.key.clone(), vec![value]);
            }
        }
    }

    fn add(&mut self, line: &[u8]) {
        if line.iter().all(|byte| matches!(byte, b' ' | b'\t' | b'\r')) {
            return;
        }
        self.lines += 1;
        let fields = Fields::parse(line);
        let (Some(method), Some(uri)) = (fields.method, fields.uri) else {
            return;
        };
        self.parsed += 1;
        let uri = uri.split(|byte| *byte == b'?').next().unwrap_or_default();
        let uri = String::from_utf8_lossy(uri);
        let route = match self.groups.iter().find(|(_, regex)| regex.is_match(&uri)) {
            Some((source, _)) => source.as_str(),
            None => uri.as_ref(),
        };
        let time = fields.reqtime.and_then(duration);
        let status = fields
            .status
            .and_then(|status| std::str::from_utf8(status).ok()?.trim().parse::<u16>().ok());
        let size = fields.size.and_then(number).unwrap_or(0.0);
        if let Some(time) = time
            && !route.is_empty()
        {
            self.whole.insert(method, route, time, status, size);
        }
        let Some(at) = fields
            .msec
            .and_then(number)
            .filter(|at| self.options.in_window(*at))
        else {
            return;
        };
        let start = self.options.bucket_start(at);
        if self
            .last_bucket
            .as_ref()
            .is_none_or(|(last, _)| *last != start)
        {
            self.last_bucket = Some((start, format!("{start:.6}")));
        }
        let bucket = self
            .last_bucket
            .as_ref()
            .map_or("", |(_, name)| name.as_str());
        if let Some(time) = time
            && !route.is_empty()
        {
            match self.buckets.get_mut(bucket) {
                Some(table) => table.insert(method, route, time, status, size),
                None => {
                    let mut table = Table::default();
                    table.insert(method, route, time, status, size);
                    self.buckets.insert(bucket.to_owned(), table);
                }
            }
        }

        // 接続: 最初の要求の始まりから最後の応答まで。同じ接続の次の要求までの間隔も取る。
        if let (Some(conn), Some(time)) = (fields.conn.filter(|conn| !conn.is_empty()), time) {
            let begun = at - time;
            match self.connection_index.get(conn) {
                Some(&index) => {
                    let gap = (begun - self.connections[index].last) * 1000.0;
                    if gap >= 0.0 {
                        let bucket = bucket.to_owned();
                        self.sample(b'g', bucket.as_bytes(), gap);
                    }
                    let connection = &mut self.connections[index];
                    connection.first = connection.first.min(begun);
                    connection.last = connection.last.max(at);
                    connection.requests += 1;
                }
                None => {
                    self.connection_index
                        .insert(conn.to_vec(), self.connections.len());
                    self.connections.push(Connection {
                        first: begun,
                        last: at,
                        requests: 1,
                    });
                    match self.opened.get_mut(bucket) {
                        Some(count) => *count += 1,
                        None => {
                            self.opened.insert(bucket.to_owned(), 1);
                        }
                    }
                }
            }
        }

        // upstream: 試行ごとに、その接続先の回数・retry・connect/header/responseの時間。
        let values = [
            fields.upstream_addr,
            fields.upstream_connect,
            fields.upstream_header,
            fields.apptime,
        ]
        .map(Option::unwrap_or_default);
        for (ranges, value) in self.attempts.iter_mut().zip(values) {
            attempts(value, ranges);
        }
        let count = self.attempts[0].len();
        for index in 0..count {
            let (start, end) = self.attempts[0][index];
            let address = values[0][start..end]
                .iter()
                .map(|byte| {
                    if matches!(byte, b'"' | b'\\') {
                        b'_'
                    } else {
                        *byte
                    }
                })
                .collect::<Vec<_>>();
            if address.is_empty() || address == b"-" {
                continue;
            }
            let slot = match self.upstream_index.get(&address) {
                Some(&slot) => slot,
                None => {
                    self.upstream_index
                        .insert(address.clone(), self.upstreams.len());
                    self.upstreams.push((address.clone(), Upstream::default()));
                    self.upstreams.len() - 1
                }
            };
            let upstream = &mut self.upstreams[slot].1;
            upstream.requests += 1;
            if index + 1 < count {
                *upstream.retried.get_or_insert(0) += 1;
            }
            for (kind, which) in [(b'c', 1), (b'h', 2), (b'r', 3)] {
                if let Some(&(start, end)) = self.attempts[which].get(index)
                    && let Some(seconds) = duration(&values[which][start..end])
                {
                    self.sample(kind, &address, seconds * 1000.0);
                }
            }
        }
    }

    /// 接続の使い方（同時接続数、新規接続、接続あたりの要求数）とupstream別の要求数。
    fn write_connections(&self, out: &mut impl Write) -> std::io::Result<()> {
        let options = self.options;
        let mut busy = BTreeMap::<String, f64>::new();
        let (mut total, mut most) = (0u64, 0u64);
        for connection in &self.connections {
            let mut start = match options.begin {
                Some(begin) => connection.first.max(begin),
                None => connection.first,
            };
            loop {
                let stop = options.bucket_end(start).min(connection.last);
                if stop > start {
                    *busy
                        .entry(format!("{:.6}", options.bucket_start(start)))
                        .or_default() += stop - start;
                }
                if stop >= connection.last {
                    break;
                }
                start = stop;
            }
            total += connection.requests;
            most = most.max(connection.requests);
        }
        // 端のbucketは5秒より短いので、実際の長さで割る。
        for (bucket, seconds) in &busy {
            let at = bucket.parse::<f64>().unwrap_or_default();
            let end = options.bucket_end(at);
            let span = options.end.map_or(end, |limit| end.min(limit)) - at;
            if span > 0.0 {
                metric(
                    out,
                    "client.connections_in_use",
                    seconds / span,
                    "connections",
                    "",
                    bucket,
                )?;
            }
        }
        for (bucket, count) in &self.opened {
            metric(
                out,
                "client.connections_opened",
                *count as f64,
                "connections",
                "",
                bucket,
            )?;
        }
        if !self.connections.is_empty() {
            let connections = self.connections.len() as f64;
            metric(
                out,
                "client.connection_requests_mean",
                total as f64 / connections,
                "requests",
                "",
                "",
            )?;
            metric(
                out,
                "client.connection_requests_max",
                most as f64,
                "requests",
                "",
                "",
            )?;
            metric(
                out,
                "client.connections_opened_total",
                connections,
                "connections",
                "",
                "",
            )?;
        }
        for (address, upstream) in &self.upstreams {
            let label = format!("\"upstream\":\"{}\"", String::from_utf8_lossy(address));
            metric(
                out,
                "http.upstream_requests",
                upstream.requests as f64,
                "requests",
                &label,
                "",
            )?;
            if let Some(retried) = upstream.retried {
                metric(
                    out,
                    "http.upstream_retried_requests",
                    retried as f64,
                    "requests",
                    &label,
                    "",
                )?;
            }
        }
        Ok(())
    }

    /// 要求の間隔（bucketごと）とupstream別の時間の合計・平均・最小・最大・分位点。
    fn write_samples(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut keys = self.samples.keys().collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            let mut values = self.samples[key].clone();
            values.sort_by(f64::total_cmp);
            let (kind, rest) = (key[0], String::from_utf8_lossy(&key[1..]));
            let (name, labels, at) = match kind {
                b'g' => ("client.request_gap", String::new(), rest.as_ref()),
                _ => (
                    match kind {
                        b'c' => "http.upstream_connect_duration",
                        b'h' => "http.upstream_header_duration",
                        _ => "http.upstream_response_duration",
                    },
                    format!("\"upstream\":\"{rest}\""),
                    "",
                ),
            };
            if kind != b'g' {
                let sum = values.iter().sum::<f64>();
                metric(out, &format!("{name}_sum"), sum, "ms", &labels, "")?;
                metric(
                    out,
                    &format!("{name}_mean"),
                    sum / values.len() as f64,
                    "ms",
                    &labels,
                    "",
                )?;
                metric(out, &format!("{name}_min"), values[0], "ms", &labels, "")?;
                metric(
                    out,
                    &format!("{name}_max"),
                    values[values.len() - 1],
                    "ms",
                    &labels,
                    "",
                )?;
            }
            for (quantile, percent) in [("0.50", 50), ("0.95", 95), ("0.99", 99)] {
                let separator = if labels.is_empty() { "" } else { "," };
                metric(
                    out,
                    name,
                    nearest_rank(&values, percent),
                    "ms",
                    &format!("{labels}{separator}\"quantile\":\"{quantile}\""),
                    at,
                )?;
            }
        }
        Ok(())
    }
}

fn metric(
    out: &mut impl Write,
    name: &str,
    value: f64,
    unit: &str,
    labels: &str,
    at: &str,
) -> std::io::Result<()> {
    write!(
        out,
        "{{\"type\":\"metric\",\"name\":\"{name}\",\"value\":{value:.6},\"unit\":\"{unit}\",\"labels\":{{{labels}}}"
    )?;
    if !at.is_empty() {
        write!(out, ",\"timestamp\":{at}")?;
    }
    writeln!(out, "}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(lines: &[&str], options: &Options) -> String {
        let directory = std::env::temp_dir().join(format!(
            "isuscope-access-summary-{}-{}",
            std::process::id(),
            lines.len()
        ));
        std::fs::write(&directory, lines.join("\n")).unwrap();
        let mut out = Vec::new();
        let code = summarize(directory.to_str().unwrap(), options, &mut out).unwrap();
        std::fs::remove_file(&directory).unwrap();
        assert_eq!(code, 0);
        String::from_utf8(out).unwrap()
    }

    fn whole(output: &str) -> Vec<Vec<serde_json::Value>> {
        let line = output
            .lines()
            .find(|line| line.starts_with("whole\t"))
            .unwrap();
        serde_json::from_str(&line["whole\t".len()..]).unwrap()
    }

    #[test]
    fn routes_drop_the_query_and_follow_the_groups() {
        let options = Options {
            matching_groups: "^/user/[0-9]+$".into(),
            ..Options::default()
        };
        let output = summary(
            &[
                "method:GET\turi:/user/1?x=1\tstatus:200\tsize:10\treqtime:0.000",
                "method:GET\turi:/user/2\tstatus:404\tsize:5\treqtime:0.004",
                "method:GET\turi:/user/3\tstatus:-\tsize:-\treqtime:0.002",
                "method:POST\turi:/login\tstatus:200\treqtime:-",
            ],
            &options,
        );
        let rows = whole(&output);
        // 見出しの行と1つのroute。POST /loginは時間が無いので表に入らない（parserが飛ばした行として数える）。
        assert_eq!(rows.len(), 2);
        let row = &rows[1];
        assert_eq!(row[0], 3); // count
        assert_eq!((row[2].as_u64(), row[4].as_u64()), (Some(1), Some(1))); // 2xx, 4xx
        assert_eq!(row[7], "^/user/[0-9]+$");
        // 最小は0、分位点は最近接順位。
        assert_eq!(row[8].as_f64(), Some(0.0));
        assert_eq!(row[12].as_f64(), Some(0.002));
        assert_eq!(row[13].as_f64(), Some(0.004));
        assert_eq!(row[15].as_f64(), Some(15.0));
        assert!(output.contains("lines\t4\n"));
    }

    #[test]
    fn too_many_routes_are_counted_together() {
        let lines = (0..ROUTE_LIMIT + 3)
            .map(|index| format!("method:GET\turi:/item/{index}\treqtime:0.001"))
            .collect::<Vec<_>>();
        let lines = lines.iter().map(String::as_str).collect::<Vec<_>>();
        let rows = whole(&summary(&lines, &Options::default()));
        // 見出しの行、上限までのroute、まとめた1行。
        assert_eq!(rows.len(), 1 + ROUTE_LIMIT + 1);
        let overflow = rows.iter().find(|row| row[7] == OVERFLOW_ROUTE).unwrap();
        assert_eq!(overflow[0], 3);
    }

    #[test]
    fn upstream_attempts_split_on_both_separators() {
        let split = |value: &'static [u8]| {
            let mut ranges = Vec::new();
            attempts(value, &mut ranges);
            ranges
                .into_iter()
                .map(|(start, end)| &value[start..end])
                .collect::<Vec<_>>()
        };
        assert_eq!(
            split(b"10.0.0.9:8080, 10.0.0.1:8080 : 10.0.0.2:8080"),
            [&b"10.0.0.9:8080"[..], b"10.0.0.1:8080", b"10.0.0.2:8080"]
        );
        assert!(split(b"").is_empty());
        assert_eq!(split(b"a,"), [&b"a"[..], b""]);
    }

    #[test]
    fn nearest_rank_rounds_the_rank_up() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(
            [50, 95, 99].map(|percent| nearest_rank(&values, percent)),
            [2.0, 4.0, 4.0]
        );
        assert_eq!(nearest_rank(&[7.0], 50), 7.0);
    }
}
