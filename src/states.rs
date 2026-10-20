//! 状態ログ：アプリが状態の変化を1行ずつ書いたJSON行。
//!
//! アプリとの約束は1行に1つの変化で、`{"t":<epochマイクロ秒>,"seq":<process内の通し番号>,
//! "kind":"<対象の種類>","id":"<対象のID>","from":"<前の状態>","to":"<後の状態>",
//! "attrs":{...}}`。作られたときは`"from":null`、書き手が前の状態を知らないとき（DBの状態の列を
//! 読まずにUPDATEするときなど）は`from`を書かない。書けずに捨てた行は
//! `{"t":...,"dropped":<その回に捨てた数>}`で残す。
//! isuscopeは問題を知らないので、種類と状態の名前はアプリが書いたものをそのまま扱う。
//!
//! `state-log-delta` collectorがベンチの前後の差分をそのまま運び、run の log に残す（生の行は
//! SQLiteの索引へ入れない）。この module はその行から集計metricを作り、`states`コマンドのために
//! 一時的なin-memoryのSQLiteへ載せる。
use crate::model::{BenchmarkResult, Metric};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io::BufRead,
    path::{Path, PathBuf},
};

/// 配布する設定で状態ログを運ぶcollector。`parser = "state-log"`を持つ他の名前のcollectorも読む。
pub const DELTA_COLLECTOR: &str = "state-log-delta";

/// 作られた（`"from":null`）遷移を、labelでこう書く。
pub const CREATED: &str = "-";
/// 書き手が前の状態を知らず、ログからも分からない遷移を、labelと`from_state`でこう書く。
pub const UNKNOWN: &str = "?";

/// 変わる前の状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Previous {
    /// `"from":null`：この行で作られた。
    Created,
    Known(String),
    /// `from`が無い：書き手が前の状態を知らない。
    Unknown,
}

impl Previous {
    fn known(&self) -> Option<&str> {
        match self {
            Previous::Known(state) => Some(state),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StateEvent {
    /// Unix時刻のマイクロ秒。
    pub t: i64,
    pub seq: i64,
    pub kind: String,
    pub id: String,
    pub from: Previous,
    pub to: String,
    pub attrs: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Event(StateEvent),
    /// 書き手があふれて捨てた行の数（その回の分）。
    Dropped(u64),
    /// 空行、またはcollectorが同じstdoutへ出した`{"type":...}`の行。
    Other,
    Invalid,
}

/// 1行の形。型が合わない値は行ごと[`Line::Invalid`]にする（時刻の小数、入れ子でない`attrs`など）。
#[derive(serde::Deserialize)]
struct RawLine {
    #[serde(rename = "type", default)]
    record: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    t: Option<i64>,
    #[serde(default)]
    seq: Option<i64>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    /// 無い（`None`）とnull（`Some(None)`）を分ける。
    #[serde(default, deserialize_with = "present")]
    from: Option<Option<String>>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    attrs: Option<Map<String, Value>>,
    #[serde(default)]
    dropped: Option<serde_json::Number>,
}

fn present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error> {
    use serde::Deserialize;
    Option::<String>::deserialize(deserializer).map(Some)
}

pub fn parse_line(bytes: &[u8]) -> Line {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Line::Other;
    }
    let line = match serde_json::from_slice::<RawLine>(bytes) {
        Ok(line) => line,
        // collectorが同じstdoutへ出したrecordは、状態ログの項目名と型が合わなくても飛ばす。
        Err(_) => {
            return match serde_json::from_slice::<Value>(bytes) {
                Ok(Value::Object(fields)) if fields.contains_key("type") => Line::Other,
                _ => Line::Invalid,
            };
        }
    };
    if line.record.is_some() {
        return Line::Other;
    }
    if line.kind.is_none()
        && let Some(dropped) = line.dropped
    {
        return dropped.as_u64().map_or(Line::Invalid, Line::Dropped);
    }
    // 整数のIDもそのまま使えるように文字列へ直す。
    let id = match line.id {
        Some(Value::String(id)) => Some(id),
        Some(Value::Number(id)) => Some(id.to_string()),
        _ => None,
    };
    let from = match line.from {
        None => Previous::Unknown,
        Some(None) => Previous::Created,
        Some(Some(from)) => Previous::Known(from),
    };
    let (Some(t), Some(kind), Some(id), Some(to)) = (
        line.t,
        line.kind.filter(|kind| !kind.is_empty()),
        id.filter(|id| !id.is_empty()),
        line.to,
    ) else {
        return Line::Invalid;
    };
    Line::Event(StateEvent {
        t,
        seq: line.seq.unwrap_or(0),
        kind,
        id,
        from,
        to,
        attrs: line.attrs,
    })
}

/// 集計する区間（マイクロ秒、`[start, end)`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub name: &'static str,
    pub start: i64,
    pub end: i64,
}

impl Window {
    /// initializeの終わりが分かるrunは負荷走行（`load`）、分からないrunはベンチ全体（`whole`）。
    pub fn of(benchmark: &BenchmarkResult) -> Option<Self> {
        let end = benchmark.finished_at?.timestamp_micros();
        let (name, start) = match benchmark.initialize_finished_at {
            Some(load) => ("load", load),
            None => ("whole", benchmark.started_at?),
        };
        Some(Self {
            name,
            start: start.timestamp_micros(),
            end,
        })
    }
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub events: Vec<StateEvent>,
    pub lines: u64,
    pub invalid_lines: u64,
    pub dropped: u64,
}

pub fn read(reader: impl BufRead) -> Parsed {
    let mut parsed = Parsed::default();
    // Split on bytes so one line that is not UTF-8 is counted instead of ending the read.
    for line in reader.split(b'\n').map_while(std::result::Result::ok) {
        match parse_line(&line) {
            Line::Event(event) => {
                parsed.lines += 1;
                parsed.events.push(event);
            }
            Line::Dropped(count) => parsed.dropped += count,
            Line::Other => {}
            Line::Invalid => parsed.invalid_lines += 1,
        }
    }
    // 同じ時刻の行は通し番号で決める。並べ替えは安定なので、それも同じなら書いた順。
    parsed.events.sort_by_key(|event| (event.t, event.seq));
    parsed
}

/// 1台の状態ログの差分から集計metricを作る。`window`が無ければ、ログの最初から最後までを区間にする。
///
/// - 遷移の回数は区間の中の行だけを数える。区間の前の行も、対象の状態を追うためには使う。
/// - 状態にいた時間（stay）は、状態が変わった行から次に変わった行まで。同じ状態への遷移は区切らない。
///   最初の行が`from`を持つ対象は、それ以前にいつ入ったか分からないので、その状態の時間には数えない。
///   区間の中で出たstayだけを数え、区間の終わりでまだ出ていないものは`state.dwell.open`に数える。
/// - 状態にいる対象の数は、ログに現れた対象だけを数える。最初の行が`from`を持つ対象は、
///   ログの始まりからその状態にいたとみなす。`from`の無い行が最初の対象は、その行から数える。
/// - 遷移の`from` labelは、行の`from`。行に無ければ追ってきた状態、それも無ければ`?`。
/// - 行の`from`が追ってきた状態と違えば（行の欠けや順序の入れ替わり）、または作られた対象がまた
///   作られれば`state.inconsistent`に数え、追ってきた状態から`to`へ移ったとして続ける。
pub fn metrics(parsed: &Parsed, window: Option<Window>) -> Vec<Metric> {
    let mut output = Vec::new();
    let mut push = |name: &str,
                    value: f64,
                    unit: &str,
                    labels: &[(&str, &str)],
                    timestamp: Option<DateTime<Utc>>| {
        output.push(Metric {
            name: name.into(),
            value,
            unit: unit.into(),
            timestamp,
            labels: labels
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        });
    };
    let window = window.or_else(|| {
        let first = parsed.events.first()?.t;
        let last = parsed.events.last()?.t;
        Some(Window {
            name: "whole",
            start: first,
            end: last + 1,
        })
    });

    let mut transitions = BTreeMap::<(&str, &str, &str), u64>::new();
    let mut stays = BTreeMap::<(&str, &str), Vec<i64>>::new();
    let mut counts = BTreeMap::<(&str, &str), i64>::new();
    let mut occupancy = BTreeMap::<(&str, &str), Vec<(i64, i64)>>::new();
    // 区間の中で、状態にいた対象の数の最大。1秒ごとの標本ではなく、変わるたびに数える。
    let mut peaks = BTreeMap::<(&str, &str), i64>::new();
    let mut inconsistent = 0_u64;

    if let Some(window) = window {
        // 状態と、その状態に入った時刻（分からなければNone）。
        let mut current = HashMap::<(&str, &str), (&str, Option<i64>)>::new();
        let mut seen = HashSet::<(&str, &str)>::new();
        for event in &parsed.events {
            if event.t >= window.end {
                break;
            }
            let key = (event.kind.as_str(), event.id.as_str());
            if seen.insert(key)
                && let Some(from) = event.from.known()
            {
                current.insert(key, (from, None));
                *counts.entry((key.0, from)).or_default() += 1;
            }
            counts.entry((key.0, event.to.as_str())).or_default();
        }
        let samples = (window.start..window.end)
            .step_by(1_000_000)
            .collect::<Vec<_>>();
        let mut next_sample = 0;
        /// `until`より前の1秒ごとの時刻に、その時点の対象の数を残す。
        fn sample<'a>(
            counts: &BTreeMap<(&'a str, &'a str), i64>,
            samples: &[i64],
            until: i64,
            next_sample: &mut usize,
            occupancy: &mut BTreeMap<(&'a str, &'a str), Vec<(i64, i64)>>,
        ) {
            while *next_sample < samples.len() && samples[*next_sample] < until {
                for (pair, count) in counts {
                    occupancy
                        .entry(*pair)
                        .or_default()
                        .push((samples[*next_sample], *count));
                }
                *next_sample += 1;
            }
        }
        let mut window_started = false;
        for event in &parsed.events {
            if event.t >= window.end {
                break;
            }
            sample(&counts, &samples, event.t, &mut next_sample, &mut occupancy);
            let in_window = event.t >= window.start;
            if in_window && !window_started {
                window_started = true;
                for (pair, count) in &counts {
                    let peak = peaks.entry(*pair).or_default();
                    *peak = (*peak).max(*count);
                }
            }
            let kind = event.kind.as_str();
            let to = event.to.as_str();
            let key = (kind, event.id.as_str());
            let tracked = current.get(&key).copied();
            if in_window {
                let from = match &event.from {
                    Previous::Created => CREATED,
                    Previous::Known(from) => from.as_str(),
                    Previous::Unknown => tracked.map_or(UNKNOWN, |(state, _)| state),
                };
                *transitions.entry((kind, from, to)).or_default() += 1;
            }
            // 書き手が前の状態を知らない行は、追ってきた状態と比べない。
            let consistent = |state: &str| match &event.from {
                Previous::Known(from) => from == state,
                Previous::Created => false,
                Previous::Unknown => true,
            };
            match tracked {
                Some((state, _)) if state == to => {
                    if !consistent(state) {
                        inconsistent += 1;
                    }
                }
                Some((state, entered)) => {
                    if !consistent(state) {
                        inconsistent += 1;
                    }
                    if in_window && let Some(entered) = entered {
                        stays
                            .entry((kind, state))
                            .or_default()
                            .push(event.t - entered);
                    }
                    *counts.entry((kind, state)).or_default() -= 1;
                    *counts.entry((kind, to)).or_default() += 1;
                    current.insert(key, (to, Some(event.t)));
                }
                None => {
                    *counts.entry((kind, to)).or_default() += 1;
                    current.insert(key, (to, Some(event.t)));
                }
            }
            if in_window {
                let count = counts.get(&(kind, to)).copied().unwrap_or_default();
                let peak = peaks.entry((kind, to)).or_default();
                *peak = (*peak).max(count);
            }
        }
        if !window_started {
            for (pair, count) in &counts {
                peaks.insert(*pair, *count);
            }
        }
        sample(
            &counts,
            &samples,
            window.end,
            &mut next_sample,
            &mut occupancy,
        );

        let mut open = BTreeMap::<(&str, &str), u64>::new();
        for ((kind, _), (state, _)) in &current {
            *open.entry((*kind, *state)).or_default() += 1;
        }
        let name = window.name;
        for ((kind, from, to), count) in &transitions {
            push(
                "state.transitions",
                *count as f64,
                "transitions",
                &[
                    ("kind", *kind),
                    ("from", *from),
                    ("to", *to),
                    ("window", name),
                ],
                None,
            );
        }
        for ((kind, state), durations) in &mut stays {
            durations.sort_unstable();
            let labels = [("kind", *kind), ("state", *state), ("window", name)];
            let ms = |micros: i64| micros as f64 / 1_000.0;
            let quantile = |quantile: f64| {
                let rank = (quantile * durations.len() as f64).ceil() as usize;
                ms(durations[rank.saturating_sub(1).min(durations.len() - 1)])
            };
            push(
                "state.dwell.count",
                durations.len() as f64,
                "stays",
                &labels,
                None,
            );
            push(
                "state.dwell.total_duration",
                ms(durations.iter().sum()),
                "ms",
                &labels,
                None,
            );
            push(
                "state.dwell.p50_duration",
                quantile(0.50),
                "ms",
                &labels,
                None,
            );
            push(
                "state.dwell.p95_duration",
                quantile(0.95),
                "ms",
                &labels,
                None,
            );
            push(
                "state.dwell.p99_duration",
                quantile(0.99),
                "ms",
                &labels,
                None,
            );
            push(
                "state.dwell.max_duration",
                ms(*durations.last().unwrap_or(&0)),
                "ms",
                &labels,
                None,
            );
        }
        for ((kind, state), count) in &open {
            push(
                "state.dwell.open",
                *count as f64,
                "objects",
                &[("kind", *kind), ("state", *state), ("window", name)],
                None,
            );
        }
        for ((kind, state), peak) in &peaks {
            push(
                "state.occupancy_max",
                *peak as f64,
                "objects",
                &[("kind", *kind), ("state", *state), ("window", name)],
                None,
            );
        }
        for ((kind, state), values) in &occupancy {
            for (at, count) in values {
                push(
                    "state.occupancy",
                    *count as f64,
                    "objects",
                    &[("kind", *kind), ("state", *state)],
                    DateTime::from_timestamp_micros(*at),
                );
            }
        }
    }
    push("state.lines", parsed.lines as f64, "lines", &[], None);
    push(
        "state.invalid_lines",
        parsed.invalid_lines as f64,
        "lines",
        &[],
        None,
    );
    push("state.dropped", parsed.dropped as f64, "events", &[], None);
    push(
        "state.inconsistent",
        inconsistent as f64,
        "events",
        &[],
        None,
    );
    output
}

/// collectorが保存したstdout（zstd）から集計metricを作る。
pub fn parse_collector_output(path: &Path, window: Option<Window>) -> Result<Vec<Metric>> {
    let input = std::fs::File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(input)?;
    Ok(metrics(&read(std::io::BufReader::new(decoder)), window))
}

/// あるrunの状態ログ。nodeごとの、collectorが保存したstdoutのlog。
pub struct Source {
    pub node: String,
    pub path: PathBuf,
}

/// `collectors`は`parser = "state-log"`のcollectorの名前（設定が無いときは[`DELTA_COLLECTOR`]）。
pub fn sources(
    manifest: &crate::model::RunManifest,
    logs_dir: &Path,
    collectors: &BTreeSet<String>,
) -> Vec<Source> {
    manifest
        .logs
        .iter()
        .filter(|log| {
            log.kind
                .strip_prefix("collector:")
                .and_then(|rest| rest.strip_suffix(":stdout"))
                .is_some_and(|name| collectors.contains(name))
        })
        .map(|log| Source {
            node: log.node.clone().unwrap_or_else(|| "local".into()),
            path: logs_dir.join(format!("{}.zst", log.id)),
        })
        .filter(|source| source.path.is_file())
        .collect()
}

pub struct Loaded {
    pub connection: Connection,
    pub events: usize,
    pub invalid_lines: u64,
    pub dropped: u64,
}

/// `states`コマンドが読む表。`--schema`でこのまま出す（`state_stays`は行を入れた後に作る）。
macro_rules! state_tables {
    () => {
        "\
CREATE TABLE state_events (
  node TEXT NOT NULL,
  t INTEGER NOT NULL,          -- Unix時刻のマイクロ秒
  at TEXT NOT NULL,            -- tをRFC 3339（UTC、マイクロ秒）にしたもの
  since_start_ms REAL,         -- ベンチの始まりからのミリ秒
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL,
  id TEXT NOT NULL,
  from_state TEXT,             -- 作られたときはNULL、書き手が前の状態を知らないときは'?'
  to_state TEXT NOT NULL,
  attrs TEXT                   -- JSON object。json_extract(attrs, '$.name')で読む
);
-- attrsの値（文字列・数・真偽）を1つずつ。別の対象のIDなど、attrsで行を結ぶときに索引が効く。
-- 例: 椅子ごとに次のMATCHEDを探す
--   SELECT MIN(a.t) FROM state_attrs a WHERE a.key = 'chair' AND a.value = :chair
--     AND a.kind = 'ride' AND a.to_state = 'MATCHED' AND a.t > :t
CREATE TABLE state_attrs (
  node TEXT NOT NULL,
  t INTEGER NOT NULL,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL,
  id TEXT NOT NULL,
  to_state TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL           -- 数と真偽も文字列（1、1.5、true）
);
-- 集計metricと同じ区間（loadかwhole）。ベンチの時刻が分からないrunでは空。
CREATE TABLE state_window (name TEXT NOT NULL, start_t INTEGER NOT NULL, end_t INTEGER NOT NULL);
"
    };
}

/// 状態にいた時間。行を入れた後に一度だけ計算して表にする（viewだと参照のたびに全行を並べ直す）。
macro_rules! state_stays {
    () => {
        "\
-- 状態にいた時間。状態が変わった行から、同じ対象の次に変わった行まで（同じ状態への遷移は区切らない）。
-- まだ出ていなければleft_tとduration_msはNULL。
CREATE TABLE state_stays AS
SELECT node, kind, id, to_state AS state, t AS entered_t,
  LEAD(t) OVER object AS left_t,
  LEAD(to_state) OVER object AS next_state,
  (LEAD(t) OVER object - t) / 1000.0 AS duration_ms,
  attrs
FROM state_events
WHERE from_state IS NULL OR from_state <> to_state
WINDOW object AS (PARTITION BY kind, id ORDER BY t, seq);
"
    };
}

/// 索引は行を入れた後に作る（1行ごとに索引を更新するより速い）。
macro_rules! state_indexes {
    () => {
        "\
CREATE INDEX state_events_object ON state_events (kind, id, t, seq);
CREATE INDEX state_events_time ON state_events (t, seq);
CREATE INDEX state_events_state ON state_events (kind, to_state, t);
CREATE INDEX state_attrs_value ON state_attrs (key, value, kind, to_state, t);
CREATE INDEX state_stays_state ON state_stays (kind, state, entered_t);
CREATE INDEX state_stays_object ON state_stays (kind, id, entered_t);
"
    };
}

pub const SCHEMA: &str = concat!(state_tables!(), state_stays!(), state_indexes!());

/// 重い問い合わせが止まったときに添える、速く書き直す方法。
pub const SLOW_QUERY_HINT: &str = "rewrite it so each row is found through an index instead of \
scanning: join on state_attrs (key, value, kind, to_state, t) instead of json_extract in a \
correlated subquery, filter state_stays by (kind, state, entered_t), and count objects over \
time with a running sum (SUM(d) OVER (ORDER BY t) over +1 at entered_t and -1 at left_t) \
instead of counting the stays again for every row";

/// あるrunの状態ログを、読み取り専用のin-memory SQLiteへ載せる。
pub fn load(
    sources: &[Source],
    window: Option<Window>,
    started_at: Option<DateTime<Utc>>,
) -> Result<Loaded> {
    let mut connection = Connection::open_in_memory()?;
    let (events, invalid_lines, dropped) = build(&mut connection, sources, window, started_at)?;
    connection.pragma_update(None, "query_only", true)?;
    Ok(Loaded {
        connection,
        events,
        invalid_lines,
        dropped,
    })
}

/// 残すrunの数。1 runのSQLiteは状態ログの数倍（40万行・64 MBのlogで数百MB）になる。
const CACHE_RUNS: usize = 3;

/// [`load`]と同じSQLiteを`cache_dir`へrunごとに残し、同じrunの次の問い合わせではそれを開く。
/// 展開・解析・表と索引の構築（40万行で約4秒）を、問い合わせのたびに繰り返さないため。
/// 元のlog、区間、isuscopeの版と表の形が変われば作り直す。作れなければin-memoryで読む。
pub fn load_cached(
    cache_dir: &Path,
    run_id: &str,
    sources: &[Source],
    window: Option<Window>,
    started_at: Option<DateTime<Utc>>,
) -> Result<Loaded> {
    let key = cache_key(sources, window, started_at);
    let path = cache_dir.join(format!("{run_id}.sqlite3"));
    if let Some(loaded) = open_cached(&path, &key) {
        // 残すrunは最近使った順に選ぶ。
        let _ = std::fs::File::options()
            .append(true)
            .open(&path)
            .and_then(|file| file.set_modified(std::time::SystemTime::now()));
        return Ok(loaded);
    }
    match build_cache(cache_dir, run_id, &path, &key, sources, window, started_at) {
        Ok(Some(loaded)) => Ok(loaded),
        _ => load(sources, window, started_at),
    }
}

fn cache_key(
    sources: &[Source],
    window: Option<Window>,
    started_at: Option<DateTime<Utc>>,
) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(env!("CARGO_PKG_VERSION"));
    hasher.update(SCHEMA);
    for source in sources {
        let metadata = std::fs::metadata(&source.path).ok();
        let modified = metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|at| at.as_nanos());
        hasher.update(format!(
            "\0{}\0{}\0{:?}\0{:?}",
            source.node,
            source.path.display(),
            metadata.map(|metadata| metadata.len()),
            modified
        ));
    }
    hasher.update(format!(
        "\0{:?}\0{:?}",
        window.map(|window| (window.name, window.start, window.end)),
        started_at.map(|at| at.timestamp_micros())
    ));
    format!("{:x}", hasher.finalize())
}

fn open_cached(path: &Path, key: &str) -> Option<Loaded> {
    if !path.is_file() {
        return None;
    }
    let connection =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let (stored, events, invalid_lines, dropped) = connection
        .query_row(
            "SELECT key, events, invalid_lines, dropped FROM isuscope_cache",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .ok()?;
    if stored != key {
        return None;
    }
    connection.pragma_update(None, "query_only", true).ok()?;
    Some(Loaded {
        connection,
        events: events as usize,
        invalid_lines: invalid_lines as u64,
        dropped: dropped as u64,
    })
}

/// 一時fileに作り切ってから置き換える。同じrunを2つのprocessが同時に作っても、読む側は
/// 作りかけを開かない（後に置いた方が残るだけ）。
fn build_cache(
    cache_dir: &Path,
    run_id: &str,
    path: &Path,
    key: &str,
    sources: &[Source],
    window: Option<Window>,
    started_at: Option<DateTime<Utc>>,
) -> Result<Option<Loaded>> {
    std::fs::create_dir_all(cache_dir)?;
    // data directoryはGitで持つことがあるので、作り直せるcacheは入れない。
    let ignore = cache_dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n")?;
    }
    let temporary = cache_dir.join(format!(".{run_id}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temporary);
    let built = (|| -> Result<()> {
        let mut connection = Connection::open(&temporary)?;
        connection.pragma_update(None, "journal_mode", "OFF")?;
        connection.pragma_update(None, "synchronous", "OFF")?;
        let (events, invalid_lines, dropped) = build(&mut connection, sources, window, started_at)?;
        connection.execute_batch(
            "CREATE TABLE isuscope_cache (key TEXT NOT NULL, events INTEGER NOT NULL, \
             invalid_lines INTEGER NOT NULL, dropped INTEGER NOT NULL)",
        )?;
        connection.execute(
            "INSERT INTO isuscope_cache VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![key, events as i64, invalid_lines as i64, dropped as i64],
        )?;
        Ok(())
    })();
    if let Err(error) = built {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    std::fs::rename(&temporary, path)?;
    evict(cache_dir, path);
    Ok(open_cached(path, key))
}

/// 最近使った[`CACHE_RUNS`]個を残し、残りと、残った一時file（10分より前）を消す。
fn evict(cache_dir: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let mut caches = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".tmp") {
            if now
                .duration_since(modified)
                .is_ok_and(|age| age > std::time::Duration::from_secs(600))
            {
                let _ = std::fs::remove_file(&path);
            }
        } else if name.ends_with(".sqlite3") && path != current {
            caches.push((modified, path));
        }
    }
    caches.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in caches.into_iter().skip(CACHE_RUNS - 1) {
        let _ = std::fs::remove_file(path);
    }
}

/// 表を作り、logの行を入れ、`state_stays`と索引を作る。行数、読めなかった行、書き手が落とした行を返す。
fn build(
    connection: &mut Connection,
    sources: &[Source],
    window: Option<Window>,
    started_at: Option<DateTime<Utc>>,
) -> Result<(usize, u64, u64)> {
    connection.execute_batch(state_tables!())?;
    let mut events = 0;
    let mut invalid_lines = 0;
    let mut dropped = 0;
    let started = started_at.map(|at| at.timestamp_micros());
    {
        let transaction = connection.transaction()?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO state_events (node, t, at, since_start_ms, seq, kind, id, from_state, to_state, attrs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            let mut insert_attr = transaction.prepare(
                "INSERT INTO state_attrs (node, t, seq, kind, id, to_state, key, value)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for source in sources {
                let input = std::fs::File::open(&source.path)
                    .with_context(|| format!("cannot open {}", source.path.display()))?;
                let decoder = zstd::stream::read::Decoder::new(input)?;
                let parsed = read(std::io::BufReader::new(decoder));
                invalid_lines += parsed.invalid_lines;
                dropped += parsed.dropped;
                for event in parsed.events {
                    let at = DateTime::from_timestamp_micros(event.t)
                        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
                        .unwrap_or_default();
                    for (key, value) in event.attrs.iter().flatten() {
                        let value = match value {
                            Value::String(text) => text.clone(),
                            Value::Number(number) => number.to_string(),
                            Value::Bool(flag) => flag.to_string(),
                            _ => continue,
                        };
                        insert_attr.execute(rusqlite::params![
                            source.node,
                            event.t,
                            event.seq,
                            event.kind,
                            event.id,
                            event.to,
                            key,
                            value,
                        ])?;
                    }
                    insert.execute(rusqlite::params![
                        source.node,
                        event.t,
                        at,
                        started.map(|started| (event.t - started) as f64 / 1_000.0),
                        event.seq,
                        event.kind,
                        event.id,
                        match &event.from {
                            Previous::Created => None,
                            Previous::Known(from) => Some(from.as_str()),
                            Previous::Unknown => Some(UNKNOWN),
                        },
                        event.to,
                        event.attrs.map(|attrs| Value::Object(attrs).to_string()),
                    ])?;
                    events += 1;
                }
            }
            if let Some(window) = window {
                transaction.execute(
                    "INSERT INTO state_window (name, start_t, end_t) VALUES (?1, ?2, ?3)",
                    rusqlite::params![window.name, window.start, window.end],
                )?;
            }
        }
        transaction.commit()?;
    }
    connection.execute_batch(concat!(state_stays!(), state_indexes!()))?;
    Ok((events, invalid_lines, dropped))
}

/// `--around`の時刻。RFC 3339（時差を付ける）、Unix時刻の秒（小数可）、またはマイクロ秒の整数。
pub fn parse_time(value: &str) -> Result<i64> {
    if let Ok(at) = DateTime::parse_from_rfc3339(value) {
        return Ok(at.timestamp_micros());
    }
    if let Ok(micros) = value.parse::<i64>()
        && micros > 100_000_000_000_000
    {
        return Ok(micros);
    }
    if let Ok(seconds) = value.parse::<f64>()
        && seconds.is_finite()
        && seconds > 0.0
    {
        return Ok((seconds * 1_000_000.0).round() as i64);
    }
    anyhow::bail!(
        "cannot read time `{value}`; use RFC 3339 with an offset (2026-10-12T09:10:47.5+09:00), Unix seconds, or Unix microseconds"
    )
}

/// `--span`の長さ（`200ms`、`1.5s`、`500us`）。マイクロ秒で返す。
pub fn parse_span(value: &str) -> Result<i64> {
    let value = value.trim();
    let (number, scale) = if let Some(number) = value.strip_suffix("us") {
        (number, 1.0)
    } else if let Some(number) = value.strip_suffix("ms") {
        (number, 1_000.0)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000_000.0)
    } else {
        anyhow::bail!("span `{value}` needs a unit: us, ms or s (for example 200ms)");
    };
    let number = number
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite() && *number >= 0.0)
        .with_context(|| format!("cannot read span `{value}`"))?;
    Ok((number * scale).round() as i64)
}

/// `states timeline`のSQL。対象の行と、`related`なら関係する対象の行を時刻順に並べる。
///
/// 関係する対象は、対象の行の`attrs`に値として現れるIDを持つ対象と、`attrs`の値に対象のIDを
/// 持つ行。`around`があればその前後`span`、無ければ対象の最初の行の`span`前から最後の行の`span`後まで。
pub const TIMELINE: &str = "
WITH target AS (
  SELECT * FROM state_events WHERE kind = :kind AND id = :id
),
bounds AS (
  SELECT COALESCE(:around - :span, MIN(t) - :span) AS lo,
         COALESCE(:around + :span, MAX(t) + :span) AS hi
  FROM target
),
related_ids AS (
  SELECT DISTINCT CAST(j.value AS TEXT) AS id
  FROM target, json_each(target.attrs) AS j
  WHERE target.attrs IS NOT NULL AND j.type IN ('text', 'integer')
),
picked AS (
  SELECT 'target' AS relation, target.* FROM target, bounds
  WHERE :around IS NULL OR target.t BETWEEN bounds.lo AND bounds.hi
  UNION ALL
  SELECT 'related' AS relation, e.* FROM state_events AS e, bounds
  WHERE :related AND NOT (e.kind = :kind AND e.id = :id)
    AND e.t BETWEEN bounds.lo AND bounds.hi
    AND (e.id IN (SELECT id FROM related_ids)
      OR (e.attrs IS NOT NULL AND EXISTS (
        SELECT 1 FROM json_each(e.attrs) AS a WHERE CAST(a.value AS TEXT) = :id)))
)
SELECT relation, at, since_start_ms,
  (t - LAG(t) OVER (ORDER BY t, seq, node)) / 1000.0 AS gap_ms,
  node, kind, id, from_state, to_state, attrs
FROM picked
ORDER BY t, seq, node
";

#[cfg(test)]
mod tests {
    use super::*;

    fn source(directory: &Path, lines: &[String]) -> Source {
        let path = directory.join("state.zst");
        std::fs::write(
            &path,
            zstd::encode_all((lines.join("\n") + "\n").as_bytes(), 1).unwrap(),
        )
        .unwrap();
        Source {
            node: "app".into(),
            path,
        }
    }

    #[test]
    fn the_cache_is_rebuilt_when_the_log_changes_and_keeps_the_latest_runs() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        let one = [event(1, "ride", "r1", None, "MATCHING")];
        let sources = [source(directory.path(), &one)];
        let first = load_cached(&cache, "run-a", &sources, None, None).unwrap();
        assert_eq!(first.events, 1);
        drop(first);
        assert!(cache.join("run-a.sqlite3").is_file());
        assert!(cache.join(".gitignore").is_file());
        // 同じlogなら開くだけ。読み取り専用。
        let again = load_cached(&cache, "run-a", &sources, None, None).unwrap();
        assert_eq!(again.events, 1);
        assert!(
            again
                .connection
                .execute("DELETE FROM state_events", [])
                .is_err()
        );
        drop(again);
        // logが変われば作り直す。
        std::thread::sleep(std::time::Duration::from_millis(20));
        let two = [
            event(1, "ride", "r1", None, "MATCHING"),
            event(2, "ride", "r1", Some("MATCHING"), "MATCHED"),
        ];
        let sources = [source(directory.path(), &two)];
        assert_eq!(
            load_cached(&cache, "run-a", &sources, None, None)
                .unwrap()
                .events,
            2
        );
        // 残すのは最近使った3 runまで。
        for run in ["run-b", "run-c", "run-d"] {
            std::thread::sleep(std::time::Duration::from_millis(20));
            load_cached(&cache, run, &sources, None, None).unwrap();
        }
        let mut kept = std::fs::read_dir(&cache)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".sqlite3"))
            .collect::<Vec<_>>();
        kept.sort();
        assert_eq!(kept, ["run-b.sqlite3", "run-c.sqlite3", "run-d.sqlite3"]);
    }

    fn event(t: i64, kind: &str, id: &str, from: Option<&str>, to: &str) -> String {
        serde_json::json!({"t": t, "seq": t, "kind": kind, "id": id, "from": from, "to": to})
            .to_string()
    }

    /// 書き手が前の状態を知らない行（`from`が無い）。
    fn entered(t: i64, kind: &str, id: &str, to: &str) -> String {
        serde_json::json!({"t": t, "seq": t, "kind": kind, "id": id, "to": to}).to_string()
    }

    fn value(metrics: &[Metric], name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        metrics
            .iter()
            .filter(|metric| metric.timestamp.is_none())
            .find(|metric| {
                metric.name == name
                    && labels.iter().all(|(key, value)| {
                        metric.labels.get(*key).map(String::as_str) == Some(value)
                    })
            })
            .map(|metric| metric.value)
    }

    #[test]
    fn lines_follow_the_contract_and_the_rest_is_counted() {
        assert!(matches!(
            parse_line(br#"{"t":1,"seq":2,"kind":"ride","id":7,"from":null,"to":"MATCHING","attrs":{"chair":"c1"}}"#),
            Line::Event(StateEvent { ref id, from: Previous::Created, .. }) if id == "7"
        ));
        assert_eq!(parse_line(br#"{"t":1,"dropped":3}"#), Line::Dropped(3));
        assert_eq!(
            parse_line(br#"{"type":"metric","name":"x","value":1}"#),
            Line::Other
        );
        assert_eq!(parse_line(b"   "), Line::Other);
        // 時刻がミリ秒の小数、toが無い、attrsが入れ子でないobject以外、壊れた行。
        for line in [
            &br#"{"t":1.5,"kind":"ride","id":"r","to":"A"}"#[..],
            br#"{"t":1,"kind":"ride","id":"r"}"#,
            br#"{"t":1,"kind":"ride","id":"r","to":"A","attrs":[1]}"#,
            br#"{"t":1,"kind":"ride","id":"r","to":"A""#,
            b"\xff\xfe",
        ] {
            assert_eq!(
                parse_line(line),
                Line::Invalid,
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn dwell_transitions_and_occupancy_follow_each_object() {
        // 区間は1,000,000〜4,000,000µs（3秒）。r1は区間の前に作られ、区間の中で2回変わる。
        // r2は最初の行がfromを持つ（ログの前からMATCHINGにいた）。r3は区間の終わりでまだENROUTE。
        let lines = [
            event(500_000, "ride", "r1", None, "MATCHING"),
            event(1_500_000, "ride", "r1", Some("MATCHING"), "ENROUTE"),
            event(1_600_000, "ride", "r2", Some("MATCHING"), "ENROUTE"),
            // 同じ状態への遷移はstayを区切らない。
            event(1_700_000, "ride", "r1", Some("ENROUTE"), "ENROUTE"),
            event(2_500_000, "ride", "r1", Some("ENROUTE"), "COMPLETED"),
            event(2_600_000, "ride", "r2", Some("ENROUTE"), "COMPLETED"),
            event(3_000_000, "ride", "r3", None, "MATCHING"),
            // fromが追ってきた状態と違う（行の欠け）。
            event(3_200_000, "ride", "r3", Some("PICKUP"), "ENROUTE"),
            // 区間の後の行は数えない。
            event(4_500_000, "ride", "r3", Some("ENROUTE"), "COMPLETED"),
            r#"{"t":3300000,"dropped":2}"#.into(),
            "not json".into(),
        ]
        .join("\n");
        let parsed = read(std::io::Cursor::new(lines));
        let metrics = metrics(
            &parsed,
            Some(Window {
                name: "load",
                start: 1_000_000,
                end: 4_000_000,
            }),
        );
        fn ride(state: &str) -> [(&str, &str); 3] {
            [("kind", "ride"), ("state", state), ("window", "load")]
        }
        assert_eq!(value(&metrics, "state.lines", &[]), Some(9.0));
        assert_eq!(value(&metrics, "state.invalid_lines", &[]), Some(1.0));
        assert_eq!(value(&metrics, "state.dropped", &[]), Some(2.0));
        assert_eq!(value(&metrics, "state.inconsistent", &[]), Some(1.0));
        // r1のMATCHINGは区間の前に入ったが、区間の中で出たので数える（1,000ms）。
        // r2のMATCHINGは入った時刻が分からないので数えない。
        assert_eq!(
            value(&metrics, "state.dwell.count", &ride("MATCHING")),
            Some(2.0)
        );
        assert_eq!(
            value(&metrics, "state.dwell.total_duration", &ride("MATCHING")),
            Some(1_000.0 + 200.0)
        );
        // r1のENROUTEは1,500,000から2,500,000まで（同じ状態への遷移で区切らない）。
        assert_eq!(
            value(&metrics, "state.dwell.count", &ride("ENROUTE")),
            Some(2.0)
        );
        assert_eq!(
            value(&metrics, "state.dwell.max_duration", &ride("ENROUTE")),
            Some(1_000.0)
        );
        assert_eq!(
            value(&metrics, "state.dwell.open", &ride("ENROUTE")),
            Some(1.0)
        );
        assert_eq!(
            value(&metrics, "state.dwell.open", &ride("COMPLETED")),
            Some(2.0)
        );
        assert_eq!(
            value(
                &metrics,
                "state.transitions",
                &[("kind", "ride"), ("from", CREATED), ("to", "MATCHING")]
            ),
            Some(1.0),
            "the creation before the window is not counted"
        );
        assert_eq!(
            value(
                &metrics,
                "state.transitions",
                &[("kind", "ride"), ("from", "ENROUTE"), ("to", "COMPLETED")]
            ),
            Some(2.0)
        );
        // 1秒ごとの対象の数。1,000,000ではr1とr2がMATCHING、2,000,000ではr1とr2がENROUTE。
        let occupancy = |state: &str, at: i64| {
            metrics
                .iter()
                .find(|metric| {
                    metric.name == "state.occupancy"
                        && metric.labels["state"] == state
                        && metric.timestamp == DateTime::from_timestamp_micros(at)
                })
                .map(|metric| metric.value)
        };
        assert_eq!(occupancy("MATCHING", 1_000_000), Some(2.0));
        assert_eq!(occupancy("ENROUTE", 2_000_000), Some(2.0));
        assert_eq!(occupancy("COMPLETED", 3_000_000), Some(2.0));
        assert_eq!(
            occupancy("COMPLETED", 4_000_000),
            None,
            "the window ends before 4s"
        );
        assert_eq!(
            value(&metrics, "state.occupancy_max", &ride("ENROUTE")),
            Some(2.0)
        );
    }

    #[test]
    fn a_writer_that_does_not_know_the_previous_state_still_closes_the_stay() {
        // r1はMATCHINGで作られ、前の状態を書かない行でENROUTEへ移る。r9は前の状態も作られた
        // 行も無いまま現れる。
        let lines = [
            event(1_100_000, "ride", "r1", None, "MATCHING"),
            entered(1_200_000, "ride", "r1", "ENROUTE"),
            entered(1_300_000, "ride", "r9", "COMPLETED"),
        ]
        .join("\n");
        let metrics = metrics(
            &read(std::io::Cursor::new(lines)),
            Some(Window {
                name: "load",
                start: 1_000_000,
                end: 3_000_000,
            }),
        );
        let transition = |from: &str, to: &str| {
            value(
                &metrics,
                "state.transitions",
                &[("kind", "ride"), ("from", from), ("to", to)],
            )
        };
        assert_eq!(transition("MATCHING", "ENROUTE"), Some(1.0));
        assert_eq!(transition(UNKNOWN, "COMPLETED"), Some(1.0));
        assert_eq!(value(&metrics, "state.inconsistent", &[]), Some(0.0));
        assert_eq!(
            value(
                &metrics,
                "state.dwell.total_duration",
                &[("kind", "ride"), ("state", "MATCHING")]
            ),
            Some(100.0)
        );
        assert_eq!(
            value(
                &metrics,
                "state.occupancy_max",
                &[("kind", "ride"), ("state", "COMPLETED")]
            ),
            Some(1.0)
        );
    }

    #[test]
    fn without_a_window_the_log_itself_is_the_window() {
        let lines = [
            event(10, "chair", "c1", None, "IDLE"),
            event(20, "chair", "c1", Some("IDLE"), "BUSY"),
        ]
        .join("\n");
        let metrics = metrics(&read(std::io::Cursor::new(lines)), None);
        assert_eq!(
            value(
                &metrics,
                "state.dwell.total_duration",
                &[("kind", "chair"), ("state", "IDLE"), ("window", "whole")]
            ),
            Some(0.01)
        );
    }

    #[test]
    fn times_and_spans_need_their_units() {
        assert_eq!(parse_span("200ms").unwrap(), 200_000);
        assert_eq!(parse_span("1.5s").unwrap(), 1_500_000);
        assert_eq!(parse_span("500us").unwrap(), 500);
        assert!(parse_span("200").is_err());
        assert_eq!(
            parse_time("2026-10-12T09:10:47.5+09:00").unwrap(),
            DateTime::parse_from_rfc3339("2026-10-12T00:10:47.5Z")
                .unwrap()
                .timestamp_micros()
        );
        assert_eq!(parse_time("1760259047.5").unwrap(), 1_760_259_047_500_000);
        assert_eq!(
            parse_time("1760259047500000").unwrap(),
            1_760_259_047_500_000
        );
        assert!(parse_time("2026-10-12T09:10:47.5").is_err());
    }
}
