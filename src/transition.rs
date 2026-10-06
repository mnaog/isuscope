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
    source: String,
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
        if let Some(rule) = self.rules.iter().find(|rule| rule.source == uri) {
            return rule.replace.clone();
        }
        // alpは束ねたmatching group（下の`alp_matching_groups`）をそのままuri欄に出す。
        if let Some((_, replace)) = self
            .alp_groups()
            .into_iter()
            .find(|(group, _)| group == uri)
        {
            return replace.to_owned();
        }
        normalize(uri, &self.rules)
    }

    /// alpへ渡すmatching groupと、その置換先。隣り合った規則が同じrouteへ置換するなら、
    /// `(A)|(B)`の1つのgroupに束ねる（alp 1.0.21は`?`を含むgroupを扱えないので`(?:`は使えない）。別々のgroupにすると、alpはそれぞれで分位点を
    /// 出し、同じrouteの分位点を正しく合わせられない。隣り合っていない規則は束ねない
    /// （間の規則より先に一致するようになり、最初に一致した規則で決まるrouteが変わる）。
    fn alp_groups(&self) -> Vec<(String, &str)> {
        let mut groups: Vec<(Vec<&str>, &str)> = Vec::new();
        for rule in &self.rules {
            match groups.last_mut() {
                Some((sources, replace)) if *replace == rule.replace => sources.push(&rule.source),
                _ => groups.push((vec![&rule.source], &rule.replace)),
            }
        }
        groups
            .into_iter()
            .map(|(sources, replace)| {
                let group = if sources.len() == 1 {
                    sources[0].to_owned()
                } else {
                    sources
                        .iter()
                        .map(|source| format!("({source})"))
                        .collect::<Vec<_>>()
                        .join("|")
                };
                (group, replace)
            })
            .collect()
    }

    pub fn alp_matching_groups(&self) -> Result<String> {
        for rule in &self.rules {
            if rule.source.contains(',') {
                anyhow::bail!(
                    "ALP matching-group pattern cannot contain a comma: {}",
                    rule.source
                );
            }
            // alp 1.0.21は`?`を含むgroup（`(?:`、`x?`、`+?`）に一致させられず、該当する要求を
            // 素のURIごとの行に分けてしまう（Dockerのalpで確かめた）。
            if rule.source.contains('?') {
                anyhow::bail!(
                    "ALP matching-group pattern cannot contain `?`; write optional parts as alternatives: {}",
                    rule.source
                );
            }
            if rule.replace.contains('$') {
                anyhow::bail!(
                    "ALP matching groups require a canonical replacement without captures: {} -> {}",
                    rule.source,
                    rule.replace
                );
            }
        }
        Ok(self
            .alp_groups()
            .into_iter()
            .map(|(group, _)| group)
            .collect::<Vec<_>>()
            .join(","))
    }
}

#[derive(Debug)]
struct Event {
    at: DateTime<Utc>,
    route: String,
    end: DateTime<Utc>,
    precise: bool,
}

/// survey-runで持ち帰った生のaccess logから、session単位の遷移を作る。route別の集計、時系列、
/// 接続とupstreamの値はnode上のalp collectorが作るので、ここでは遷移だけを扱う。
pub struct TransitionOptions<'a> {
    pub run_dir: &'a Path,
    pub prefix: &'a str,
    pub rules: Option<&'a Path>,
    pub time_field: &'a str,
    pub session_field: &'a str,
    pub method_field: &'a str,
    pub uri_field: &'a str,
}

pub fn emit(options: TransitionOptions<'_>) -> Result<usize> {
    let rules = RouteNormalizer::load(options.rules)?;
    let mut paths = find_logs(options.run_dir, options.prefix)?;
    paths.sort();
    let mut sessions: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    for path in paths {
        read_log(&path, &options, &rules, &mut sessions)?;
    }
    let (edges, quality) = summarize(&mut sessions);
    for row in &edges {
        println!("{}", serde_json::to_string(row)?);
    }
    for row in quality {
        println!("{}", serde_json::to_string(&row)?);
    }

    Ok(edges.len())
}

fn summarize(
    sessions: &mut BTreeMap<String, Vec<Event>>,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    type Edge = (Vec<f64>, usize, usize);
    let mut edges: BTreeMap<(String, String), Edge> = BTreeMap::new();
    let mut precise = 0;
    let mut legacy = 0;
    for events in sessions.values_mut() {
        events.sort_by_key(|event| (event.at, event.end));
        for event in events.iter() {
            if event.precise {
                precise += 1;
            } else {
                legacy += 1;
            }
        }
        for pair in events.windows(2) {
            let edge = edges
                .entry((pair[0].route.clone(), pair[1].route.clone()))
                .or_default();
            edge.0.push(
                (pair[1].at - pair[0].at)
                    .num_microseconds()
                    .unwrap_or_default() as f64
                    / 1000.0,
            );
            edge.1 += usize::from(pair[0].precise && pair[1].precise && pair[1].at < pair[0].end);
            edge.2 += usize::from(!pair[0].precise || !pair[1].precise || pair[0].at == pair[1].at);
        }
    }
    let mut transitions = Vec::new();
    let mut quality = vec![
        json!({"type":"metric","name":"transition.ordering_version","value":2,"unit":"version"}),
        json!({"type":"metric","name":"transition.precise_events","value":precise,"unit":"events"}),
        json!({"type":"metric","name":"transition.legacy_events","value":legacy,"unit":"events"}),
    ];
    for ((from, to), (mut durations, overlap, ambiguous)) in edges {
        durations.sort_by(f64::total_cmp);
        transitions.push(
            json!({"type":"transition","from":from,"to":to,"count":durations.len(),
            "p50_ms":percentile(&durations,0.50),"p95_ms":percentile(&durations,0.95)}),
        );
        for (name, value) in [
            ("transition.overlap_count", overlap),
            ("transition.ambiguous_count", ambiguous),
        ] {
            quality.push(json!({"type":"metric","name":name,"value":value,"unit":"transitions","labels":{"from":from,"to":to}}));
        }
    }
    (transitions, quality)
}

// nginx records completion time in $msec. Subtract $request_time to order starts.
// Older logs remain readable, but their ordering is explicitly marked ambiguous.
fn event_time(
    fields: &BTreeMap<&str, &str>,
    time_field: &str,
) -> Option<(DateTime<Utc>, DateTime<Utc>, bool)> {
    if let (Some(end), Some(seconds)) = (
        fields.get("msec").and_then(|v| parse_timestamp(v)),
        fields.get("reqtime").and_then(|v| v.parse::<f64>().ok()),
    ) && seconds.is_finite()
        && (0.0..=86_400.0).contains(&seconds)
    {
        let duration = chrono::Duration::microseconds((seconds * 1_000_000.0).round() as i64);
        if let Some(start) = end.checked_sub_signed(duration) {
            return Some((start, end, true));
        }
    }
    let at = fields.get(time_field).and_then(|v| parse_timestamp(v))?;
    Some((at, at, false))
}

fn read_log(
    path: &Path,
    options: &TransitionOptions<'_>,
    rules: &RouteNormalizer,
    sessions: &mut BTreeMap<String, Vec<Event>>,
) -> Result<()> {
    let input = fs::File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(input)?;
    let reader = BufReader::new(decoder);
    // 行はあるのにmethodとuriを1件も取れなければ、log formatと設定が合っていない。
    let mut lines = 0_u64;
    let mut parsed = 0_u64;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;
        let fields = parse_ltsv(&line);
        let (Some(method), Some(uri)) = (
            fields.get(options.method_field),
            fields.get(options.uri_field),
        ) else {
            continue;
        };
        parsed += 1;
        let Some(session) = fields
            .get(options.session_field)
            .filter(|session| !session.is_empty() && **session != "-")
        else {
            continue;
        };
        let Some((at, end, precise)) = event_time(&fields, options.time_field) else {
            continue;
        };
        let route = rules.normalize(uri.split('?').next().unwrap_or(uri));
        sessions
            .entry((*session).to_owned())
            .or_default()
            .push(Event {
                at,
                end,
                precise,
                route: format!("{method} {route}"),
            });
    }
    if lines > 0 && parsed == 0 {
        anyhow::bail!(
            "{}: none of {lines} access-log lines had the `{}` and `{}` fields",
            path.display(),
            options.method_field,
            options.uri_field
        );
    }
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
                source: rule.pattern.clone(),
                pattern: Regex::new(&rule.pattern)
                    .with_context(|| format!("invalid route pattern `{}`", rule.pattern))?,
                replace: rule.replace,
            })
        })
        .collect()
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

    fn options(dir: &Path) -> TransitionOptions<'_> {
        TransitionOptions {
            run_dir: dir,
            prefix: "nginx-",
            rules: None,
            time_field: "time",
            session_field: "session",
            method_field: "method",
            uri_field: "uri",
        }
    }

    fn write_log(dir: &Path, lines: &[&str]) {
        let logs = dir.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let output = fs::File::create(logs.join("nginx-isu1.zst")).unwrap();
        let mut encoder = zstd::stream::write::Encoder::new(output, 1).unwrap();
        for line in lines {
            writeln!(encoder, "{line}").unwrap();
        }
        encoder.finish().unwrap();
    }

    #[test]
    fn orders_by_start_and_marks_overlap_and_legacy() {
        let make = |line: &str, route: &str| {
            let fields = parse_ltsv(line);
            let (at, end, precise) = event_time(&fields, "time").unwrap();
            Event {
                at,
                end,
                precise,
                route: route.into(),
            }
        };
        let mut sessions = BTreeMap::from([(
            "a".into(),
            vec![
                make("msec:100.300\treqtime:0.100", "B"),
                make("msec:100.900\treqtime:0.800", "A"),
                make("msec:101.100\treqtime:0.100", "C"),
            ],
        )]);
        let (edges, quality) = summarize(&mut sessions);
        assert_eq!(edges[0]["from"], "A");
        assert_eq!(edges[0]["to"], "B");
        assert_eq!(edges[0]["p50_ms"], 100.0);
        assert!(
            quality
                .iter()
                .any(|v| v["name"] == "transition.overlap_count"
                    && v["labels"]["from"] == "A"
                    && v["value"] == 1)
        );
        sessions.insert(
            "legacy".into(),
            vec![make("time:100", "X"), make("time:100", "Y")],
        );
        let (_, quality) = summarize(&mut sessions);
        assert!(
            quality
                .iter()
                .any(|v| v["name"] == "transition.ambiguous_count"
                    && v["labels"]["from"] == "X"
                    && v["value"] == 1)
        );
        assert!(
            quality
                .iter()
                .any(|v| v["name"] == "transition.legacy_events" && v["value"] == 2)
        );
    }

    #[test]
    fn reads_session_transitions_across_compressed_logs() {
        let dir = tempdir().unwrap();
        write_log(
            dir.path(),
            &[
                "time:2026-08-26T10:00:00+09:00\tsession:a\tmethod:GET\turi:/api/user/nao/icon",
                "time:2026-08-26T10:00:01+09:00\tsession:a\tmethod:GET\turi:/api/livestream/42?x=1",
                "time:2026-08-26T10:00:01+09:00\tsession:-\tmethod:GET\turi:/api/tag",
            ],
        );
        let rules = RouteNormalizer {
            rules: vec![
                RouteRule {
                    source: r"^/api/user/[^/]+/icon$".into(),
                    pattern: Regex::new(r"^/api/user/[^/]+/icon$").unwrap(),
                    replace: "/api/user/:name/icon".into(),
                },
                RouteRule {
                    source: r"^/api/livestream/[0-9]+$".into(),
                    pattern: Regex::new(r"^/api/livestream/[0-9]+$").unwrap(),
                    replace: "/api/livestream/:id".into(),
                },
            ],
        };
        let mut sessions = BTreeMap::new();
        read_log(
            &dir.path().join("logs/nginx-isu1.zst"),
            &options(dir.path()),
            &rules,
            &mut sessions,
        )
        .unwrap();
        assert_eq!(sessions.len(), 1);
        let events = sessions.get("a").unwrap();
        assert_eq!(events[0].route, "GET /api/user/:name/icon");
        assert_eq!(events[1].route, "GET /api/livestream/:id");
    }

    #[test]
    fn access_log_without_method_or_uri_fails_instead_of_reporting_nothing() {
        let dir = tempdir().unwrap();
        // combined形式のまま（LTSVでない）logを渡した場合。
        write_log(
            dir.path(),
            &["127.0.0.1 - - [26/Aug/2026:10:00:00 +0900] \"GET / HTTP/1.1\" 200 12"],
        );
        let error = emit(options(dir.path())).unwrap_err();
        assert!(error.to_string().contains("none of 1 access-log lines"));
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
    }
    #[test]
    fn alp_matching_groups_preserve_exact_route_percentiles() {
        let rules = RouteNormalizer {
            rules: vec![RouteRule {
                source: r"^/api/livestream/[0-9]+/reaction$".into(),
                pattern: Regex::new(r"^/api/livestream/[0-9]+/reaction$").unwrap(),
                replace: "/api/livestream/:id/reaction".into(),
            }],
        };
        assert_eq!(
            rules.alp_matching_groups().unwrap(),
            r"^/api/livestream/[0-9]+/reaction$"
        );
        // ALP returns the matching regexp in its uri column. The adapter must
        // translate that value to the same canonical route as raw access logs.
        assert_eq!(
            rules.normalize(r"^/api/livestream/[0-9]+/reaction$"),
            "/api/livestream/:id/reaction"
        );
        assert_eq!(
            rules.normalize("/api/livestream/42/reaction"),
            "/api/livestream/:id/reaction"
        );
    }

    #[test]
    fn alp_matching_groups_bundle_adjacent_rules_for_the_same_route() {
        let rule = |source: &str, replace: &str| RouteRule {
            source: source.into(),
            pattern: Regex::new(source).unwrap(),
            replace: replace.into(),
        };
        let rules = RouteNormalizer {
            rules: vec![
                rule(r"^/items/[0-9]+$", "/items/:key"),
                rule(r"^/items/[a-z]+$", "/items/:key"),
                rule(r"^/users/[0-9]+$", "/users/:id"),
                rule(r"^/items/[A-Z]+$", "/items/:key"),
            ],
        };
        let groups = rules.alp_matching_groups().unwrap();
        // 隣り合う2つは1つのgroupに束ね、alpが1行で分位点を出す。離れた規則は束ねない。
        assert_eq!(
            groups,
            r"(^/items/[0-9]+$)|(^/items/[a-z]+$),^/users/[0-9]+$,^/items/[A-Z]+$"
        );
        assert_eq!(
            rules.normalize(r"(^/items/[0-9]+$)|(^/items/[a-z]+$)"),
            "/items/:key"
        );
        assert_eq!(rules.normalize(r"^/items/[A-Z]+$"), "/items/:key");
        assert_eq!(rules.normalize("/items/abc"), "/items/:key");
    }

    #[test]
    fn alp_matching_groups_reject_ambiguous_rules() {
        let captured = RouteNormalizer {
            rules: vec![RouteRule {
                source: r"^/api/(user|livestream)/([^/]+)$".into(),
                pattern: Regex::new(r"^/api/(user|livestream)/([^/]+)$").unwrap(),
                replace: "/api/$1/:id".into(),
            }],
        };
        assert!(captured.alp_matching_groups().is_err());

        let comma = RouteNormalizer {
            rules: vec![RouteRule {
                source: r"^/api/(foo,bar)$".into(),
                pattern: Regex::new(r"^/api/(foo,bar)$").unwrap(),
                replace: "/api/:name".into(),
            }],
        };
        assert!(comma.alp_matching_groups().is_err());

        let optional = RouteNormalizer {
            rules: vec![RouteRule {
                source: r"^/api/tags/?$".into(),
                pattern: Regex::new(r"^/api/tags/?$").unwrap(),
                replace: "/api/tags".into(),
            }],
        };
        assert!(optional.alp_matching_groups().is_err());
    }
}
