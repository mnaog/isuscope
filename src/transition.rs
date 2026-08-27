use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
pub struct RouteRules {
    #[serde(default)]
    routes: Vec<RouteRuleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct RouteRuleConfig {
    pattern: String,
    replace: String,
}

struct RouteRule {
    pattern: Regex,
    replace: String,
}

pub struct RouteNormalizer {
    rules: Vec<RouteRule>,
}

impl RouteNormalizer {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        Ok(Self {
            rules: load_rules(path)?,
        })
    }

    pub fn normalize(&self, uri: &str) -> String {
        normalize(uri, &self.rules)
    }
}

#[derive(Debug)]
struct Event {
    at: DateTime<Utc>,
    route: String,
}

#[derive(Default)]
struct RouteStats {
    requests_by_status: BTreeMap<String, u64>,
    request_durations_ms: Vec<f64>,
    upstream_durations_ms: Vec<f64>,
    response_bytes: u64,
    reused_requests: u64,
}

const MAX_ROUTE_SERIES: usize = 1_024;

pub struct TransitionOptions<'a> {
    pub run_dir: &'a Path,
    pub prefix: &'a str,
    pub rules: Option<&'a Path>,
    pub time_field: &'a str,
    pub session_field: &'a str,
    pub method_field: &'a str,
    pub uri_field: &'a str,
    pub status_field: &'a str,
    pub request_time_field: &'a str,
    pub upstream_time_field: &'a str,
    pub bytes_field: &'a str,
    pub connection_requests_field: &'a str,
}

pub fn emit(options: TransitionOptions<'_>) -> Result<usize> {
    let rules = RouteNormalizer::load(options.rules)?;
    let mut paths = find_logs(options.run_dir, options.prefix)?;
    paths.sort();
    let mut sessions: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    let mut route_stats: BTreeMap<(String, String, String), RouteStats> = BTreeMap::new();
    for path in paths {
        let node = node_from_path(&path, options.prefix);
        read_log(
            &path,
            &node,
            &options,
            &rules,
            &mut sessions,
            &mut route_stats,
        )?;
    }

    let mut edges: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
    for events in sessions.values_mut() {
        events.sort_by_key(|event| event.at);
        for pair in events.windows(2) {
            let duration_ms = (pair[1].at - pair[0].at)
                .num_microseconds()
                .unwrap_or_default() as f64
                / 1_000.0;
            edges
                .entry((pair[0].route.clone(), pair[1].route.clone()))
                .or_default()
                .push(duration_ms);
        }
    }
    for ((from, to), durations) in &mut edges {
        durations.sort_by(f64::total_cmp);
        println!(
            "{}",
            serde_json::to_string(&json!({
                "type": "transition",
                "from": from,
                "to": to,
                "count": durations.len(),
                "p50_ms": percentile(durations, 0.50),
                "p95_ms": percentile(durations, 0.95),
            }))?
        );
    }
    emit_route_metrics(&mut route_stats)?;
    Ok(edges.len())
}

fn emit_route_metrics(
    route_stats: &mut BTreeMap<(String, String, String), RouteStats>,
) -> Result<()> {
    for ((node, method, route), stats) in route_stats {
        let base_labels: BTreeMap<String, String> = BTreeMap::from([
            ("node".into(), node.clone()),
            ("method".into(), method.clone()),
            ("route".into(), route.clone()),
        ]);
        let mut errors = 0_u64;
        for (status_class, count) in &stats.requests_by_status {
            let mut labels = base_labels.clone();
            labels.insert("status_class".into(), status_class.clone());
            emit_metric("http.requests", *count as f64, "requests", labels)?;
            if matches!(status_class.as_str(), "4xx" | "5xx") {
                errors += count;
            }
        }
        emit_metric(
            "http.errors",
            errors as f64,
            "requests",
            base_labels.clone(),
        )?;
        emit_metric(
            "http.response_bytes",
            stats.response_bytes as f64,
            "bytes",
            base_labels.clone(),
        )?;
        emit_metric(
            "http.connection_reused_requests",
            stats.reused_requests as f64,
            "requests",
            base_labels.clone(),
        )?;
        emit_quantiles(
            "http.request_duration",
            &mut stats.request_durations_ms,
            &base_labels,
        )?;
        emit_quantiles(
            "http.upstream_duration",
            &mut stats.upstream_durations_ms,
            &base_labels,
        )?;
    }
    Ok(())
}

fn emit_quantiles(name: &str, values: &mut [f64], labels: &BTreeMap<String, String>) -> Result<()> {
    values.sort_by(f64::total_cmp);
    for (quantile, label) in [(0.50, "0.50"), (0.95, "0.95"), (0.99, "0.99")] {
        if let Some(value) = percentile(values, quantile) {
            let mut labels = labels.clone();
            labels.insert("quantile".into(), label.into());
            emit_metric(name, value, "ms", labels)?;
        }
    }
    Ok(())
}

fn emit_metric(name: &str, value: f64, unit: &str, labels: BTreeMap<String, String>) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(&json!({
            "type": "metric",
            "name": name,
            "value": value,
            "unit": unit,
            "labels": labels,
        }))?
    );
    Ok(())
}

fn find_logs(run_dir: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let logs = run_dir.join("logs");
    let mut paths = Vec::new();
    for entry in fs::read_dir(&logs)
        .with_context(|| format!("cannot read log directory {}", logs.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(prefix) && name.ends_with(".zst") {
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

fn load_rules(path: Option<&Path>) -> Result<Vec<RouteRule>> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let raw = fs::read_to_string(path)
        .with_context(|| format!("cannot read route rules {}", path.display()))?;
    let config: RouteRules =
        toml::from_str(&raw).with_context(|| format!("invalid route rules {}", path.display()))?;
    config
        .routes
        .into_iter()
        .map(|rule| {
            Ok(RouteRule {
                pattern: Regex::new(&rule.pattern)
                    .with_context(|| format!("invalid route pattern `{}`", rule.pattern))?,
                replace: rule.replace,
            })
        })
        .collect()
}

fn read_log(
    path: &Path,
    node: &str,
    options: &TransitionOptions<'_>,
    rules: &RouteNormalizer,
    sessions: &mut BTreeMap<String, Vec<Event>>,
    route_stats: &mut BTreeMap<(String, String, String), RouteStats>,
) -> Result<()> {
    let input = fs::File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(input)?;
    let reader = BufReader::new(decoder);
    for line in reader.lines() {
        let line = line?;
        let fields = parse_ltsv(&line);
        let Some(method) = fields.get(options.method_field) else {
            continue;
        };
        let Some(uri) = fields.get(options.uri_field) else {
            continue;
        };
        let uri = uri.split('?').next().unwrap_or(uri);
        let route = rules.normalize(uri);
        let mut stats_key = (node.to_owned(), (*method).to_owned(), route.clone());
        if !route_stats.contains_key(&stats_key) && route_stats.len() >= MAX_ROUTE_SERIES {
            stats_key.2 = "/__cardinality_limit__".into();
        }
        let stats = route_stats.entry(stats_key).or_default();
        let status_class = fields
            .get(options.status_field)
            .map(|status| status_class(status))
            .unwrap_or_else(|| "unknown".into());
        *stats.requests_by_status.entry(status_class).or_default() += 1;
        if let Some(value) = fields
            .get(options.request_time_field)
            .and_then(|value| parse_seconds_ms(value))
        {
            stats.request_durations_ms.push(value);
        }
        if let Some(value) = fields
            .get(options.upstream_time_field)
            .and_then(|value| parse_upstream_seconds_ms(value))
        {
            stats.upstream_durations_ms.push(value);
        }
        stats.response_bytes += fields
            .get(options.bytes_field)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();
        if fields
            .get(options.connection_requests_field)
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|value| value > 1)
        {
            stats.reused_requests += 1;
        }

        let Some(session) = fields.get(options.session_field) else {
            continue;
        };
        if session.is_empty() || *session == "-" {
            continue;
        }
        let Some(time) = fields.get(options.time_field) else {
            continue;
        };
        let Some(at) = parse_timestamp(time) else {
            continue;
        };
        sessions
            .entry((*session).to_owned())
            .or_default()
            .push(Event {
                at,
                route: format!("{method} {route}"),
            });
    }
    Ok(())
}

fn node_from_path(path: &Path, prefix: &str) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    let value = name.strip_prefix(prefix).unwrap_or(name);
    for suffix in ["-after-stdout.zst", "-during-stdout.zst", ".zst"] {
        if let Some(value) = value.strip_suffix(suffix) {
            return value.to_owned();
        }
    }
    value.to_owned()
}

fn status_class(status: &str) -> String {
    status
        .chars()
        .next()
        .filter(char::is_ascii_digit)
        .map(|first| format!("{first}xx"))
        .unwrap_or_else(|| "unknown".into())
}

fn parse_seconds_ms(value: &str) -> Option<f64> {
    value.parse::<f64>().ok().map(|value| value * 1_000.0)
}

fn parse_upstream_seconds_ms(value: &str) -> Option<f64> {
    let values = value
        .split([',', ':'])
        .filter_map(|part| part.trim().parse::<f64>().ok())
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.into_iter().sum::<f64>() * 1_000.0)
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(value) = DateTime::parse_from_rfc3339(value) {
        return Some(value.with_timezone(&Utc));
    }
    if let Ok(value) = DateTime::parse_from_str(value, "%d/%b/%Y:%H:%M:%S %z") {
        return Some(value.with_timezone(&Utc));
    }
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole = whole.parse::<i64>().ok()?;
    if !fraction.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let mut nanos = 0_u32;
    let mut place = 100_000_000_u32;
    for digit in fraction.bytes().take(9) {
        nanos += u32::from(digit - b'0') * place;
        place /= 10;
    }
    DateTime::from_timestamp(whole, nanos)
}

fn percentile(sorted: &[f64], quantile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (quantile * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.saturating_sub(1)).copied()
}

fn parse_ltsv(line: &str) -> BTreeMap<&str, &str> {
    line.split('\t')
        .filter_map(|field| field.split_once(':'))
        .collect()
}

fn normalize<'a>(uri: &'a str, rules: &'a [RouteRule]) -> String {
    for rule in rules {
        if rule.pattern.is_match(uri) {
            return rule
                .pattern
                .replace(uri, rule.replace.as_str())
                .into_owned();
        }
    }
    uri.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn reads_session_transitions_across_compressed_logs() {
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        let output = fs::File::create(logs.join("nginx-isu1.zst")).unwrap();
        let mut encoder = zstd::stream::write::Encoder::new(output, 1).unwrap();
        writeln!(
            encoder,
            "time:2026-08-26T10:00:00+09:00\tsession:a\tmethod:GET\turi:/api/user/nao/icon"
        )
        .unwrap();
        writeln!(
            encoder,
            "time:2026-08-26T10:00:01+09:00\tsession:a\tmethod:GET\turi:/api/livestream/42"
        )
        .unwrap();
        encoder.finish().unwrap();
        let rules = RouteNormalizer {
            rules: vec![
                RouteRule {
                    pattern: Regex::new(r"^/api/user/[^/]+/icon$").unwrap(),
                    replace: "/api/user/:name/icon".into(),
                },
                RouteRule {
                    pattern: Regex::new(r"^/api/livestream/[0-9]+$").unwrap(),
                    replace: "/api/livestream/:id".into(),
                },
            ],
        };
        let mut sessions = BTreeMap::new();
        let options = TransitionOptions {
            run_dir: dir.path(),
            prefix: "nginx-",
            rules: None,
            time_field: "time",
            session_field: "session",
            method_field: "method",
            uri_field: "uri",
            status_field: "status",
            request_time_field: "reqtime",
            upstream_time_field: "apptime",
            bytes_field: "size",
            connection_requests_field: "connreqs",
        };
        let mut route_stats = BTreeMap::new();
        read_log(
            &logs.join("nginx-isu1.zst"),
            "isu1",
            &options,
            &rules,
            &mut sessions,
            &mut route_stats,
        )
        .unwrap();
        let events = sessions.get("a").unwrap();
        assert_eq!(events[0].route, "GET /api/user/:name/icon");
        assert_eq!(events[1].route, "GET /api/livestream/:id");
        assert_eq!(route_stats.len(), 2);
    }

    #[test]
    fn parses_supported_timestamp_formats_and_percentiles() {
        assert_eq!(
            parse_timestamp("1787742994.060")
                .unwrap()
                .timestamp_millis(),
            1_787_742_994_060
        );
        assert_eq!(
            parse_timestamp("26/Aug/2026:11:16:34 +0000")
                .unwrap()
                .timestamp(),
            1_787_742_994
        );
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.50), Some(2.0));
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 0.95), Some(4.0));
        assert_eq!(status_class("304"), "3xx");
        assert_eq!(parse_upstream_seconds_ms("0.001, 0.002"), Some(3.0));
    }
}
