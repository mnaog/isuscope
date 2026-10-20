//! access logの差分を、node上で1回読んで集計する（`isuscope-node access-summary`）。
//!
//! 以前のalp collectorは、awkで差分を5秒ごとのfileに分け、upstreamと接続の値をsortで並べ、
//! 差分全体とfileごとにalpを走らせていた。2コアのnodeで62万行に約20秒かかり、行数に比例して
//! 伸びた（practice-14-seventhで322万行・87秒）。ここは同じ出力（`alp-windows` parserが読む形）を、
//! 差分全体のalp集計と、awkが担っていた区間・接続・upstreamの集計の2本のthreadで作る。
//!
//! 出力をalp 1.0.21と前のawkに合わせるため、次の挙動はそのまま写す。
//! - alpの行の読み方（go-ltsv）：`:`の無い項目や途中の空の項目があれば、その行は読まない。
//!   keyと値の前後の空白は落とす。同じkeyは後のものを使う。
//! - alpのURI（Goの`url.Parse`と`String`）：query stringを落とし、壊れた`%`escapeや制御文字の
//!   ある行は読まない。escapeが要る文字を含むpathは、Goと同じくescapeし直す。
//! - alpの値：応答時間は`reqtime`、bytesは`size`、statusは整数として読めない行は読まない。
//!   最小値は前の値が0なら次の値で上書きする（alpの`Min`）。分位点は`int((n+1)*p)-1`番目。
//!   出力する時間とbytesは小数3桁に丸める。URIが5,000種類を超えたら失敗する。
//! - awkの読み方：`:`までをkeyにし、無い項目は飛ばす。methodとuriがある行を数え、`$msec`で
//!   ベンチの区間[始まり, 終わり)に入る行だけを5秒bucketと接続・upstreamへ回す。
//!   upstream・接続の値は小数6桁に丸めてから並べ、合計と分位点を取る（sortを通した値と同じ）。
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

/// alpの`--limit`の既定。これを超える種類のURIが出たら、alpは集計をやめて失敗する。
const URI_LIMIT: usize = 5000;

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

/// ベンチの区間（空なら分からない）とalpの`--matching-groups`。
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
        let offset = (at - origin) / 5.0;
        let mut index = offset.trunc();
        if index > offset {
            index -= 1.0;
        }
        origin + (index + 1.0) * 5.0
    }

    /// bucketの先頭。ベンチの始まりをまたぐbucketは、始まりを先頭にする。
    fn bucket_start(&self, at: f64) -> f64 {
        let start = self.bucket_end(at) - 5.0;
        match self.begin {
            Some(begin) if at >= begin && start < begin => begin,
            _ => start,
        }
    }
}

/// 差分のfileを集計してoutへ書く。行はあるのにmethodとuriを1件も読めなければ65を返す。
pub fn summarize(input: &str, options: &Options, out: &mut impl Write) -> Result<i32, String> {
    let groups = compile_groups(&options.matching_groups)?;
    let open = || File::open(input).map_err(|error| format!("cannot read {input}: {error}"));
    // alpの集計（差分全体と5秒bucket）と、awkが担っていた接続・upstreamの集計を、別threadで読む。
    let (tables, windows) = std::thread::scope(|scope| {
        let tables = scope.spawn(|| -> Result<Tables, String> {
            let mut tables = Tables::new(options, &groups);
            for_each_line(open()?, |line| tables.add(line))
                .map_err(|error| format!("cannot read {input}: {error}"))?;
            Ok(tables)
        });
        let windows = (|| -> Result<Windows, String> {
            let mut windows = Windows::new(options);
            for_each_line(open()?, |line| windows.add(line))
                .map_err(|error| format!("cannot read {input}: {error}"))?;
            Ok(windows)
        })();
        (tables.join().expect("alp thread panicked"), windows)
    });
    let (tables, windows) = (tables?, windows?);
    let io = |error: std::io::Error| error.to_string();

    windows.write_connections(out).map_err(io)?;
    if windows.lines > 0 && windows.parsed == 0 {
        eprintln!(
            "none of {} access-log lines had the method and uri fields",
            windows.lines
        );
        return Ok(65);
    }
    windows.write_samples(out).map_err(io)?;
    write!(out, "whole\t").map_err(io)?;
    tables.whole.write_json(out)?;
    writeln!(out).map_err(io)?;
    writeln!(out, "lines\t{}", windows.parsed).map_err(io)?;
    for (bucket, alp) in &tables.buckets {
        write!(out, "{bucket}\t").map_err(io)?;
        alp.write_json(out)?;
        writeln!(out).map_err(io)?;
    }
    Ok(0)
}

fn compile_groups(csv: &str) -> Result<Vec<(String, Regex)>, String> {
    if csv.is_empty() {
        return Ok(Vec::new());
    }
    csv.split(',')
        .map(|source| source.trim_matches(' '))
        .map(|source| {
            Regex::new(source)
                .map(|regex| (source.to_owned(), regex))
                .map_err(|error| format!("invalid matching group {source}: {error}"))
        })
        .collect()
}

/// 改行で区切った行（末尾の改行は含めない）を順に渡す。最後の行は改行が無くても渡す。
fn for_each_line(file: File, mut each: impl FnMut(&[u8])) -> std::io::Result<()> {
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line = Vec::with_capacity(1024);
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        each(&line);
    }
}

// ---- alp（go-ltsvの読み方とalpの集計） ----

struct Route {
    method: String,
    uri: String,
    count: u64,
    statuses: [u64; 5],
    max: f64,
    min: f64,
    sum: f64,
    times: Vec<f64>,
    body_sum: f64,
}

/// alpが1行から読む値。muslのmallocは遅いので、行ごとには確保せず、使い回すbufferを指す。
struct Entry<'e> {
    /// alpが行を分けるkey（`method_uri`）。
    key: &'e [u8],
    method: &'e [u8],
    uri: &'e str,
    time: f64,
    body: f64,
    status: i64,
}

/// [`Entry`]が指す、行をまたいで使い回すbuffer。
#[derive(Default)]
struct Scratch {
    key: Vec<u8>,
    uri: String,
}

/// alpの表：差分全体と、ベンチの区間の5秒bucketごと。
struct Tables<'a> {
    options: &'a Options,
    groups: &'a [(String, Regex)],
    whole: Alp,
    buckets: BTreeMap<String, Alp>,
    names: BucketNames,
    scratch: Scratch,
}

impl<'a> Tables<'a> {
    fn new(options: &'a Options, groups: &'a [(String, Regex)]) -> Self {
        Self {
            options,
            groups,
            whole: Alp::default(),
            buckets: BTreeMap::new(),
            names: BucketNames::default(),
            scratch: Scratch::default(),
        }
    }

    fn add(&mut self, line: &[u8]) {
        let Some(entry) = Alp::entry(self.groups, line, &mut self.scratch) else {
            return;
        };
        self.whole.insert(&entry);
        // bucketのfileには、awkが区間の中と読んだ行だけが入る。
        if !is_blank(line)
            && let Some(at) = AwkFields::parse(line).window(self.options)
        {
            let bucket = self.names.name(self.options.bucket_start(at));
            match self.buckets.get_mut(bucket) {
                Some(alp) => alp.insert(&entry),
                None => {
                    let mut alp = Alp::default();
                    alp.insert(&entry);
                    self.buckets.insert(bucket.to_owned(), alp);
                }
            }
        }
    }
}

/// bucketの名前（先頭のepoch秒を小数6桁で）。同じbucketが続くので、直前の名前を使い回す。
#[derive(Default)]
struct BucketNames {
    last: Option<(f64, String)>,
}

impl BucketNames {
    fn name(&mut self, start: f64) -> &str {
        if self.last.as_ref().is_none_or(|(last, _)| *last != start) {
            self.last = Some((start, format!("{start:.6}")));
        }
        self.last.as_ref().map_or("", |(_, name)| name)
    }
}

#[derive(Default)]
struct Alp {
    index: HashMap<Vec<u8>, usize>,
    routes: Vec<Route>,
    too_many: bool,
}

impl Alp {
    /// alpが1行から読む値。読まない行は`None`。
    fn entry<'e>(
        groups: &'e [(String, Regex)],
        line: &'e [u8],
        scratch: &'e mut Scratch,
    ) -> Option<Entry<'e>> {
        // alpのreadlineは行末の\rと\nを落とし、空になった行を飛ばす。
        let mut end = line.len();
        while end > 0 && matches!(line[end - 1], b'\r' | b'\n') {
            end -= 1;
        }
        let record = LtsvRecord::parse(&line[..end])?;
        let Scratch { key, uri: buffer } = scratch;
        buffer.clear();
        if !go_uri_into(record.uri, buffer) {
            return None;
        }
        let time = go_float(record.reqtime)?;
        let body = go_float(record.size)?;
        let status = go_int(record.status)?;
        let uri = match groups.iter().find(|(_, regex)| regex.is_match(buffer)) {
            Some((source, _)) => source.as_str(),
            None => buffer.as_str(),
        };
        key.clear();
        key.extend_from_slice(record.method);
        key.push(b'_');
        key.extend_from_slice(uri.as_bytes());
        Some(Entry {
            key,
            method: record.method,
            uri,
            time,
            body,
            status,
        })
    }

    fn insert(&mut self, entry: &Entry) {
        if self.too_many {
            return;
        }
        let index = match self.index.get(entry.key) {
            Some(&index) => index,
            None => {
                self.index.insert(entry.key.to_vec(), self.routes.len());
                self.routes.push(Route {
                    method: String::from_utf8_lossy(entry.method).into_owned(),
                    uri: entry.uri.to_owned(),
                    count: 0,
                    statuses: [0; 5],
                    max: 0.0,
                    min: 0.0,
                    sum: 0.0,
                    times: Vec::new(),
                    body_sum: 0.0,
                });
                self.routes.len() - 1
            }
        };
        let (status, time) = (entry.status, entry.time);
        let route = &mut self.routes[index];
        route.count += 1;
        if (100..=599).contains(&status) {
            route.statuses[(status / 100 - 1) as usize] += 1;
        }
        if route.max < time {
            route.max = time;
        }
        if route.min >= time || route.min == 0.0 {
            route.min = time;
        }
        route.sum += time;
        route.times.push(time);
        route.body_sum += entry.body;
        if self.routes.len() > URI_LIMIT {
            self.too_many = true;
        }
    }

    /// `alp --format json`と同じ形（先頭が見出しの表）。値は小数3桁に丸めた数。
    fn write_json(&self, out: &mut impl Write) -> Result<(), String> {
        if self.too_many {
            return Err(format!("Too many URI's ({URI_LIMIT} or less)"));
        }
        let io = |error: std::io::Error| error.to_string();
        let mut order = (0..self.routes.len()).collect::<Vec<_>>();
        order.sort_by_key(|&index| self.routes[index].count);
        let mut text = String::with_capacity(64 + self.routes.len() * 160);
        text.push('[');
        text.push_str(HEADER);
        for index in order {
            let route = &self.routes[index];
            let mut times = route.times.clone();
            times.sort_by(|left, right| {
                left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
            });
            let percentile = |percent: usize| times[percent_rank(times.len(), percent)];
            let uri = if route.uri.contains(',') {
                format!("\"{}\"", route.uri)
            } else {
                route.uri.clone()
            };
            let _ = write!(text, ",[{}", route.count);
            for count in route.statuses {
                let _ = write!(text, ",{count}");
            }
            text.push(',');
            json_value(&mut text, &route.method);
            text.push(',');
            json_value(&mut text, &uri);
            for value in [
                route.min,
                route.max,
                route.sum,
                route.sum / route.count as f64,
                percentile(50),
                percentile(95),
                percentile(99),
                route.body_sum,
            ] {
                text.push(',');
                json_value(&mut text, &format!("{value:.3}"));
            }
            text.push(']');
        }
        text.push(']');
        out.write_all(text.as_bytes()).map_err(io)
    }
}

/// alpの`percentRank`。
fn percent_rank(count: usize, percent: usize) -> usize {
    match percent {
        0 => 0,
        100 => count - 1,
        _ => {
            let position = ((count + 1) as f64 * (percent as f64 / 100.0)) as usize;
            position.saturating_sub(1)
        }
    }
}

/// alpは表の各値を`json.Number`として読み、整数、小数の順に数へ直せたものを数で出す。
fn json_value(text: &mut String, value: &str) {
    if let Ok(integer) = value.parse::<i64>() {
        let _ = write!(text, "{integer}");
    } else if let Some(number) = go_float(value.as_bytes()).filter(|number| number.is_finite()) {
        let _ = write!(text, "{number}");
    } else {
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
}

/// go-ltsvで読んだ1行の、alpが使う値。無いkeyは空。
#[derive(Default)]
struct LtsvRecord<'a> {
    uri: &'a [u8],
    method: &'a [u8],
    reqtime: &'a [u8],
    size: &'a [u8],
    status: &'a [u8],
}

impl<'a> LtsvRecord<'a> {
    /// `:`の無い項目や途中の空の項目があれば、go-ltsvは行全体を読めない（alpは空のuriとして飛ばす）。
    fn parse(line: &'a [u8]) -> Option<Self> {
        if line.is_empty() {
            return None;
        }
        let mut record = Self::default();
        let mut rest = line;
        loop {
            let (field, next) = match memchr(b'\t', rest) {
                Some(tab) => (&rest[..tab], Some(&rest[tab + 1..])),
                None => (rest, None),
            };
            let colon = memchr(b':', field)?;
            let key = go_trim_space(&field[..colon]);
            let value = go_trim_space(&field[colon + 1..]);
            match key {
                b"uri" => record.uri = value,
                b"method" => record.method = value,
                b"reqtime" => record.reqtime = value,
                b"size" => record.size = value,
                b"status" => record.status = value,
                _ => {}
            }
            match next {
                // 最後のtabの後は読まない（go-ltsvは末尾のtabを項目にしない）。
                Some(next) if !next.is_empty() => rest = next,
                _ => return Some(record),
            }
        }
    }
}

fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|byte| *byte == needle)
}

/// Goの`strings.TrimSpace`。
fn go_trim_space(value: &[u8]) -> &[u8] {
    match std::str::from_utf8(value) {
        Ok(text) => text.trim().as_bytes(),
        Err(_) => value.trim_ascii(),
    }
}

/// Goの`strconv.ParseFloat`で読める値。範囲を超えた値（`1e400`）はGoでは誤りになる。
fn go_float(value: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(value).ok()?;
    let number = text.parse::<f64>().ok()?;
    if number.is_infinite() {
        let word = text.trim_start_matches(['+', '-']).to_ascii_lowercase();
        if word != "inf" && word != "infinity" {
            return None;
        }
    }
    Some(number)
}

/// Goの`strconv.Atoi`で読める値。
fn go_int(value: &[u8]) -> Option<i64> {
    std::str::from_utf8(value).ok()?.parse::<i64>().ok()
}

#[derive(Clone, Copy, PartialEq)]
enum Escape {
    Path,
    Fragment,
}

/// Goの`shouldEscape`（pathとfragment）。
fn should_escape(byte: u8, mode: Escape) -> bool {
    if byte.is_ascii_alphanumeric() {
        return false;
    }
    match byte {
        b'-' | b'_' | b'.' | b'~' => false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => {
            mode == Escape::Path && byte == b'?'
        }
        b'!' | b'(' | b')' | b'*' if mode == Escape::Fragment => false,
        _ => true,
    }
}

/// Goの`validEncoded`。escapeし直さずにそのまま出せる文字だけか。
fn valid_encoded(value: &[u8], mode: Escape) -> bool {
    value
        .iter()
        .all(|&byte| b"!$&'()*+,;=:@[]%".contains(&byte) || !should_escape(byte, mode))
}

/// `%`の後に16進2桁が続かなければGoの`unescape`は誤りにする。
fn go_unescape(value: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%' {
            let high = (*value.get(index + 1)? as char).to_digit(16)?;
            let low = (*value.get(index + 2)? as char).to_digit(16)?;
            out.push((high * 16 + low) as u8);
            index += 3;
        } else {
            out.push(value[index]);
            index += 1;
        }
    }
    Some(out)
}

fn go_escape(value: &[u8], mode: Escape) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value {
        if should_escape(byte, mode) {
            let _ = write!(out, "%{byte:02X}");
        } else {
            out.push(byte as char);
        }
    }
    out
}

/// Goが`url.Parse`の後に`String`で出す形（`EscapedPath`、`EscapedFragment`）。
fn go_escaped(raw: &[u8], mode: Escape) -> Option<String> {
    let decoded = go_unescape(raw)?;
    if valid_encoded(raw, mode) {
        return Some(String::from_utf8_lossy(raw).into_owned());
    }
    Some(go_escape(&decoded, mode))
}

/// `%`の後に必ず16進2桁が続くか。
fn valid_escapes(value: &[u8]) -> bool {
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%' {
            if !(value.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                && value.get(index + 2).is_some_and(u8::is_ascii_hexdigit))
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

/// [`go_uri`]をoutへ足す。読まない行ならfalse。nginxの`$request_uri`のほとんど（`/`で始まり、
/// Goがescapeし直さないpath）は、確保せずにそのまま写す。
fn go_uri_into(raw: &[u8], out: &mut String) -> bool {
    if raw.first() == Some(&b'/') && !raw.starts_with(b"//") && memchr(b'#', raw).is_none() {
        let (rest, force_query) = match memchr(b'?', raw) {
            Some(at) if at == raw.len() - 1 => (&raw[..at], true),
            Some(at) => (&raw[..at], false),
            None => (raw, false),
        };
        if raw.iter().all(|byte| *byte >= 0x20 && *byte != 0x7f)
            && valid_encoded(rest, Escape::Path)
            && valid_escapes(rest)
            && let Ok(path) = std::str::from_utf8(rest)
        {
            out.push_str(path);
            if force_query {
                out.push('?');
            }
            return true;
        }
    }
    match go_uri(raw) {
        Some(uri) => {
            out.push_str(&uri);
            true
        }
        None => false,
    }
}

/// alpがURIとして集計する値（Goの`url.Parse`の後、queryを落として`String`）。空なら読まない行。
///
/// `/`で始まるpath（nginxの`$request_uri`）はGoと同じに扱う。schemeやauthorityで始まる値は、
/// queryを落とすだけにする。
fn go_uri(raw: &[u8]) -> Option<String> {
    let (main, fragment) = match memchr(b'#', raw) {
        Some(at) => (&raw[..at], Some(&raw[at + 1..])),
        None => (raw, None),
    };
    if main.iter().any(|byte| *byte < 0x20 || *byte == 0x7f) {
        return None;
    }
    let mut uri = if main == b"*" {
        "*".to_owned()
    } else {
        let scheme_end = main
            .iter()
            .enumerate()
            .find(|(index, byte)| {
                !(byte.is_ascii_alphabetic()
                    || (*index > 0 && (byte.is_ascii_digit() || b"+-.".contains(byte))))
            })
            .filter(|(_, byte)| **byte == b':')
            .map(|(index, _)| index);
        if scheme_end == Some(0) {
            return None;
        }
        let (rest, force_query) = match memchr(b'?', main) {
            Some(at) if at == main.len() - 1 => (&main[..at], true),
            Some(at) => (&main[..at], false),
            None => (main, false),
        };
        let mut uri = if scheme_end.is_some() || rest.starts_with(b"//") {
            if scheme_end.is_none() && rest.starts_with(b"///") {
                go_escaped(rest, Escape::Path)?
            } else {
                String::from_utf8_lossy(rest).into_owned()
            }
        } else {
            let first_segment = rest.split(|byte| *byte == b'/').next().unwrap_or_default();
            if first_segment.contains(&b':') {
                return None;
            }
            let path = go_escaped(rest, Escape::Path)?;
            if path.split('/').next().unwrap_or_default().contains(':') {
                format!("./{path}")
            } else {
                path
            }
        };
        if force_query {
            uri.push('?');
        }
        uri
    };
    if let Some(fragment) = fragment.filter(|fragment| !fragment.is_empty()) {
        uri.push('#');
        uri.push_str(&go_escaped(fragment, Escape::Fragment)?);
    }
    (!uri.is_empty()).then_some(uri)
}

// ---- awk（区間・5秒bucket・接続・upstream） ----

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

struct Windows<'a> {
    options: &'a Options,
    lines: u64,
    parsed: u64,
    connections: HashMap<Vec<u8>, usize>,
    connection_order: Vec<Connection>,
    opened: BTreeMap<String, u64>,
    upstream_index: HashMap<Vec<u8>, usize>,
    upstreams: Vec<(Vec<u8>, Upstream)>,
    samples: Samples,
    names: BucketNames,
    /// upstreamの試行（addr、connect、header、response）を分けた範囲。行をまたいで使い回す。
    attempts: [Vec<(usize, usize)>; 4],
}

/// key（種類の1文字と、bucketかupstream）ごとの値。小数6桁を整数にしたもの。
#[derive(Default)]
struct Samples {
    values: HashMap<Vec<u8>, Vec<i64>>,
    key: Vec<u8>,
    format: String,
}

impl Samples {
    fn add(&mut self, kind: u8, rest: &[u8], value: f64) {
        let micros = self.micros(value);
        self.key.clear();
        self.key.push(kind);
        self.key.extend_from_slice(rest);
        match self.values.get_mut(&self.key[..]) {
            Some(values) => values.push(micros),
            None => {
                self.values.insert(self.key.clone(), vec![micros]);
            }
        }
    }

    /// 小数6桁に丸めた値（`printf "%.6f"`と同じ）を、10^6倍の整数で。丸めの境目（.5）から
    /// 遠い値は掛け算で決め、近い値だけ文字列にして決める。
    fn micros(&mut self, value: f64) -> i64 {
        let scaled = value * 1_000_000.0;
        let floor = scaled.floor();
        let fraction = scaled - floor;
        if scaled.abs() < 1e12 && (fraction - 0.5).abs() > 1e-3 {
            return floor as i64 + i64::from(fraction > 0.5);
        }
        self.format.clear();
        let _ = write!(self.format, "{value:.6}");
        let micros = self.format.bytes().filter(|byte| *byte != b'.').fold(
            (0i64, false),
            |(total, negative), byte| match byte {
                b'-' => (total, true),
                digit => (total * 10 + i64::from(digit - b'0'), negative),
            },
        );
        if micros.1 { -micros.0 } else { micros.0 }
    }
}

/// awkが1行から読む値。`:`までをkeyにし、同じkeyは後のものを使う。
#[derive(Default)]
struct AwkFields<'a> {
    method: bool,
    uri: bool,
    msec: Option<&'a [u8]>,
    conn: Option<&'a [u8]>,
    reqtime: Option<&'a [u8]>,
    upstream_addr: Option<&'a [u8]>,
    upstream_connect: Option<&'a [u8]>,
    upstream_header: Option<&'a [u8]>,
    apptime: Option<&'a [u8]>,
}

impl<'a> AwkFields<'a> {
    fn parse(line: &'a [u8]) -> Self {
        let mut fields = Self::default();
        for field in line.split(|byte| *byte == b'\t') {
            let Some(colon) = memchr(b':', field) else {
                continue;
            };
            let value = &field[colon + 1..];
            match &field[..colon] {
                b"method" => fields.method = true,
                b"uri" => fields.uri = true,
                b"msec" => fields.msec = Some(value),
                b"conn" => fields.conn = Some(value),
                b"reqtime" => fields.reqtime = Some(value),
                b"upstream_addr" => fields.upstream_addr = Some(value),
                b"upstream_connect" => fields.upstream_connect = Some(value),
                b"upstream_header" => fields.upstream_header = Some(value),
                b"apptime" => fields.apptime = Some(value),
                _ => {}
            }
        }
        fields
    }
}

impl AwkFields<'_> {
    /// methodとuriがあり、`$msec`がベンチの区間[始まり, 終わり)に入る行の時刻。
    fn window(&self, options: &Options) -> Option<f64> {
        if !self.method || !self.uri {
            return None;
        }
        let at = awk_number(self.msec.filter(|msec| is_epoch(msec))?);
        if options.begin.is_some_and(|begin| at < begin) || options.end.is_some_and(|end| at >= end)
        {
            return None;
        }
        Some(at)
    }
}

/// awkの`/^[ \t]*$/`。数えない行。
fn is_blank(line: &[u8]) -> bool {
    line.iter().all(|byte| matches!(byte, b' ' | b'\t'))
}

/// `/^[0-9]+(\.[0-9]+)?$/`
fn is_epoch(value: &[u8]) -> bool {
    let (integer, fraction) = match memchr(b'.', value) {
        Some(dot) => (&value[..dot], Some(&value[dot + 1..])),
        None => (value, None),
    };
    !integer.is_empty()
        && integer.iter().all(u8::is_ascii_digit)
        && fraction
            .is_none_or(|fraction| !fraction.is_empty() && fraction.iter().all(u8::is_ascii_digit))
}

/// `/^[0-9.]+$/`
fn is_duration(value: &[u8]) -> bool {
    !value.is_empty()
        && value
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b'.')
}

/// awkが`[0-9.]+`の文字列を数にした値（先頭の数として読める部分、無ければ0）。
fn awk_number(value: &[u8]) -> f64 {
    let digits = value
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let mut end = digits;
    if value.get(end) == Some(&b'.') {
        end += 1 + value[end + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
    }
    if end == 0 || (digits == 0 && end == 1) {
        return 0.0;
    }
    std::str::from_utf8(&value[..end])
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(0.0)
}

/// awkの`attempts`：` : `を`,`に直して`,`で分け、前後の空白を落とす。空なら0個。
/// 分けた各部分のvalueの中の範囲をoutへ入れる。
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
    let mut start = 0;
    let mut index = 0;
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

impl<'a> Windows<'a> {
    fn new(options: &'a Options) -> Self {
        Self {
            options,
            lines: 0,
            parsed: 0,
            connections: HashMap::new(),
            connection_order: Vec::new(),
            opened: BTreeMap::new(),
            upstream_index: HashMap::new(),
            upstreams: Vec::new(),
            samples: Samples::default(),
            names: BucketNames::default(),
            attempts: Default::default(),
        }
    }

    fn add(&mut self, line: &[u8]) {
        if is_blank(line) {
            return;
        }
        self.lines += 1;
        let fields = AwkFields::parse(line);
        if !fields.method || !fields.uri {
            return;
        }
        self.parsed += 1;
        let Some(at) = fields.window(self.options) else {
            return;
        };
        let bucket = self.names.name(self.options.bucket_start(at));

        if let (Some(conn), Some(reqtime)) = (
            fields.conn.filter(|conn| !conn.is_empty()),
            fields.reqtime.filter(|reqtime| is_duration(reqtime)),
        ) {
            let start = at - awk_number(reqtime);
            match self.connections.get(conn) {
                Some(&index) => {
                    let gap = (start - self.connection_order[index].last) * 1000.0;
                    if gap >= 0.0 {
                        self.samples.add(b'g', bucket.as_bytes(), gap);
                    }
                    let connection = &mut self.connection_order[index];
                    if start < connection.first {
                        connection.first = start;
                    }
                    if at > connection.last {
                        connection.last = at;
                    }
                    connection.requests += 1;
                }
                None => {
                    self.connections
                        .insert(conn.to_vec(), self.connection_order.len());
                    self.connection_order.push(Connection {
                        first: start,
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
        let [addresses, connects, headers, responses] = &self.attempts;
        let count = addresses.len();
        for (index, &(start, end)) in addresses.iter().enumerate() {
            let raw = &values[0][start..end];
            let address = if raw.iter().any(|byte| matches!(byte, b'"' | b'\\')) {
                std::borrow::Cow::Owned(
                    raw.iter()
                        .map(|byte| {
                            if matches!(byte, b'"' | b'\\') {
                                b'_'
                            } else {
                                *byte
                            }
                        })
                        .collect::<Vec<_>>(),
                )
            } else {
                std::borrow::Cow::Borrowed(raw)
            };
            if address.is_empty() || *address == *b"-" {
                continue;
            }
            let slot = match self.upstream_index.get(&address[..]) {
                Some(&slot) => slot,
                None => {
                    self.upstream_index
                        .insert(address.to_vec(), self.upstreams.len());
                    self.upstreams.push((address.to_vec(), Upstream::default()));
                    self.upstreams.len() - 1
                }
            };
            let upstream = &mut self.upstreams[slot].1;
            upstream.requests += 1;
            if index + 1 < count {
                *upstream.retried.get_or_insert(0) += 1;
            }
            for (kind, ranges, value) in [
                (b'c', connects, values[1]),
                (b'h', headers, values[2]),
                (b'r', responses, values[3]),
            ] {
                if let Some(&(start, end)) = ranges.get(index)
                    && is_duration(&value[start..end])
                {
                    self.samples
                        .add(kind, &address, awk_number(&value[start..end]) * 1000.0);
                }
            }
        }
    }

    /// 接続の使い方（同時接続数、新規接続、接続あたりの要求数）とupstream別の要求数。
    fn write_connections(&self, out: &mut impl Write) -> std::io::Result<()> {
        let options = self.options;
        let mut busy = BTreeMap::<String, f64>::new();
        let (mut total, mut most) = (0u64, 0u64);
        for connection in &self.connection_order {
            let mut start = connection.first;
            if let Some(begin) = options.begin
                && start < begin
            {
                start = begin;
            }
            loop {
                let bucket = format!("{:.6}", options.bucket_start(start));
                let mut stop = options.bucket_end(start);
                if connection.last < stop {
                    stop = connection.last;
                }
                if stop > start {
                    *busy.entry(bucket).or_default() += stop - start;
                }
                if stop >= connection.last {
                    break;
                }
                start = stop;
            }
            total += connection.requests;
            most = most.max(connection.requests);
        }
        for (bucket, seconds) in &busy {
            let at = bucket.parse::<f64>().unwrap_or_default();
            let mut span = options.bucket_end(at) - at;
            if let Some(end) = options.end
                && options.bucket_end(at) > end
            {
                span = end - at;
            }
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
        let connections = self.connection_order.len();
        if connections > 0 {
            metric(
                out,
                "client.connection_requests_mean",
                total as f64 / connections as f64,
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
                connections as f64,
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

    /// 要求の間隔（bucketごと）とupstream別の時間の分位点・合計。並べた値から取る。
    fn write_samples(&self, out: &mut impl Write) -> std::io::Result<()> {
        let mut keys = self.samples.values.keys().collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            let mut values = self.samples.values[key].clone();
            values.sort_unstable();
            let values = values
                .into_iter()
                .map(|micros| micros as f64 / 1_000_000.0)
                .collect::<Vec<_>>();
            let (kind, rest) = (key[0], String::from_utf8_lossy(&key[1..]));
            let (name, labels, at) = if kind == b'g' {
                ("client.request_gap", String::new(), rest.as_ref())
            } else {
                let name = match kind {
                    b'c' => "http.upstream_connect_duration",
                    b'h' => "http.upstream_header_duration",
                    _ => "http.upstream_response_duration",
                };
                (name, format!("\"upstream\":\"{rest}\""), "")
            };
            if kind != b'g' {
                let sum = values.iter().fold(0.0, |sum, value| sum + value);
                let count = values.len() as f64;
                metric(out, &format!("{name}_sum"), sum, "ms", &labels, "")?;
                metric(out, &format!("{name}_mean"), sum / count, "ms", &labels, "")?;
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
            for quantile in ["0.50", "0.95", "0.99"] {
                let rank = {
                    let rank = quantile.parse::<f64>().unwrap_or_default() * values.len() as f64;
                    let mut index = rank.trunc();
                    if index < rank {
                        index += 1.0;
                    }
                    (index as usize).max(1)
                };
                let separator = if labels.is_empty() { "" } else { "," };
                metric(
                    out,
                    name,
                    values[rank - 1],
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

    #[test]
    fn uris_follow_go_url_parse_and_string() {
        let uri = |raw: &str| go_uri(raw.as_bytes());
        assert_eq!(uri("/user/1/home?x=1").as_deref(), Some("/user/1/home"));
        assert_eq!(uri("/a?").as_deref(), Some("/a?"));
        assert_eq!(uri("/a%2Fb").as_deref(), Some("/a%2Fb"));
        assert_eq!(uri("/a b").as_deref(), Some("/a%20b"));
        assert_eq!(uri("/%E3%81%82").as_deref(), Some("/%E3%81%82"));
        assert_eq!(uri("/a\"b").as_deref(), Some("/a%22b"));
        assert_eq!(uri("/a#f").as_deref(), Some("/a#f"));
        assert_eq!(uri("/%ZZ"), None);
        assert_eq!(uri("/a\x01"), None);
        assert_eq!(uri(""), None);
        assert_eq!(uri(":x"), None);
        assert_eq!(uri("a:b/c").as_deref(), Some("a:b/c"));
    }

    #[test]
    fn ltsv_lines_follow_go_ltsv() {
        let record = LtsvRecord::parse(b"uri: /a \tstatus:200\t").unwrap();
        assert_eq!((record.uri, record.status), (&b"/a"[..], &b"200"[..]));
        assert!(LtsvRecord::parse(b"uri:/a\t\tstatus:200").is_none());
        assert!(LtsvRecord::parse(b"uri:/a\tbroken").is_none());
        let duplicate = LtsvRecord::parse(b"uri:/a\turi:/b").unwrap();
        assert_eq!(duplicate.uri, b"/b");
    }

    #[test]
    fn alp_values_keep_its_minimum_and_rank() {
        // alpの最小値は、前の値が0なら次の値で上書きする。
        let minimum = |times: [&str; 3]| {
            let mut alp = Alp::default();
            for time in times {
                let line = format!("method:GET\turi:/a\tstatus:200\tsize:1\treqtime:{time}");
                let mut scratch = Scratch::default();
                alp.insert(&Alp::entry(&[], line.as_bytes(), &mut scratch).unwrap());
            }
            alp.routes[0].min
        };
        assert_eq!(minimum(["0.000", "0.005", "0.003"]), 0.003);
        assert_eq!(minimum(["0.002", "0.000", "0.005"]), 0.005);
        assert_eq!(
            [1, 2, 10, 100].map(|count| percent_rank(count, 95)),
            [0, 1, 9, 94]
        );
    }

    #[test]
    fn upstream_attempts_split_like_the_awk() {
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
        assert_eq!(awk_number(b"1.2.3"), 1.2);
        assert_eq!(awk_number(b"."), 0.0);
    }

    #[test]
    fn three_decimal_rounding_matches_printf() {
        // C・Goと同じく、ちょうど半分は偶数へ丸める。
        assert_eq!(format!("{:.3}", 0.0625), "0.062");
        assert_eq!(format!("{:.3}", 0.0005), "0.001");
    }
}
