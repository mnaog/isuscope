use crate::model::{CollectorResult, Metric};
use std::collections::{BTreeMap, BTreeSet};

const DEFAULT_LIMIT: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Bottleneck {
    pub category: &'static str,
    pub node: String,
    pub target: String,
    pub evidence: String,
    pub source: &'static str,
    /// A category-local 0..1 score. Scores are deliberately not physical units.
    pub severity: f64,
    pub verify_metric: &'static str,
    pub strength: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    pub category: &'static str,
    pub available: bool,
    pub detail: String,
}

#[derive(Debug, PartialEq)]
pub struct Report {
    pub candidates: Vec<Bottleneck>,
    pub coverage: Vec<Coverage>,
}

#[derive(Default)]
struct HttpStats {
    requests: f64,
    p95: Option<f64>,
}
#[derive(Default)]
struct DbStats {
    calls: f64,
    total: f64,
}

/// Produces cross-category suspicions without adding unlike units together.
///
/// Each category is normalized by its dominant observation. We first retain the
/// strongest candidate from every observed category, then fill the remaining
/// slots by category-local severity. This keeps a hot endpoint from hiding a
/// saturated disk (and vice versa), while still returning a short work queue.
pub fn rank(metrics: &[Metric], collectors: &[CollectorResult]) -> Report {
    let mut by_category = BTreeMap::<&'static str, Vec<Bottleneck>>::new();
    http(metrics, &mut by_category);
    database(metrics, &mut by_category);
    cpu(metrics, &mut by_category);
    host(metrics, &mut by_category);

    for values in by_category.values_mut() {
        let max = values.iter().map(|v| v.severity).fold(0.0_f64, f64::max);
        if max > 0.0 {
            for value in values.iter_mut() {
                value.severity /= max;
            }
        }
        values.sort_by(|a, b| {
            b.severity
                .total_cmp(&a.severity)
                .then_with(|| a.target.cmp(&b.target))
        });
    }

    let categories = ["http", "database", "cpu", "host"];
    let coverage = categories
        .iter()
        .map(|category| Coverage {
            category,
            available: by_category.get(category).is_some_and(|v| !v.is_empty()),
            detail: coverage_detail(category, collectors),
        })
        .collect();
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for category in categories {
        if let Some(value) = by_category.get(category).and_then(|v| v.first()) {
            seen.insert((value.category, value.node.clone(), value.target.clone()));
            candidates.push(value.clone());
        }
    }
    let mut rest: Vec<_> = by_category
        .into_values()
        .flatten()
        .filter(|v| !seen.contains(&(v.category, v.node.clone(), v.target.clone())))
        .collect();
    rest.sort_by(|a, b| {
        b.severity
            .total_cmp(&a.severity)
            .then_with(|| a.category.cmp(b.category))
            .then_with(|| a.target.cmp(&b.target))
    });
    candidates.extend(rest);
    candidates.truncate(DEFAULT_LIMIT);
    annotate_strength(&mut candidates, metrics);
    Report {
        candidates,
        coverage,
    }
}

fn http(metrics: &[Metric], out: &mut BTreeMap<&'static str, Vec<Bottleneck>>) {
    let mut by_source = BTreeMap::<(String, String, String, String), HttpStats>::new();
    for m in metrics {
        if m.timestamp.is_some() {
            continue;
        }
        let Some(route) = m.labels.get("route") else {
            continue;
        };
        let key = (
            label(m, "node"),
            label(m, "method"),
            route.clone(),
            label(m, "collector"),
        );
        let s = by_source.entry(key).or_default();
        if m.name == "http.requests" {
            s.requests += m.value;
        }
        if m.name == "http.request_duration"
            && m.labels.get("quantile").is_some_and(|q| q == "0.95")
        {
            s.p95 = Some(m.value);
        }
    }
    let mut stats = BTreeMap::<(String, String, String), (String, HttpStats)>::new();
    for ((node, method, route, collector), value) in by_source {
        if value.requests <= 0.0 || value.p95.is_none() {
            continue;
        }
        let key = (node, method, route);
        let replace = stats.get(&key).is_none_or(|(current, _)| {
            source_priority("http", &collector) < source_priority("http", current)
        });
        if replace {
            stats.insert(key, (collector, value));
        }
    }
    for ((node, method, route), (collector, s)) in stats {
        if let Some(p95) = s.p95 {
            if s.requests > 0.0 && valid(p95) {
                out.entry("http").or_default().push(Bottleneck {
                    category: "http",
                    node,
                    target: format!("{method} {route}"),
                    evidence: format!(
                        "requests={:.0} p95={p95:.2}ms impact={:.2}ms",
                        s.requests,
                        s.requests * p95
                    ),
                    source: source_name("http", &collector),
                    severity: s.requests * p95,
                    verify_metric: "http.request_duration{quantile=0.95}",
                    strength: "summary-only",
                });
            }
        }
    }
}

fn database(metrics: &[Metric], out: &mut BTreeMap<&'static str, Vec<Bottleneck>>) {
    let mut by_source = BTreeMap::<(String, String, String, String), DbStats>::new();
    for m in metrics {
        if m.timestamp.is_some() {
            continue;
        }
        if !matches!(
            m.name.as_str(),
            "db.query.calls" | "db.query.total_duration"
        ) {
            continue;
        }
        let Some(digest) = m.labels.get("digest") else {
            continue;
        };
        let key = (
            label(m, "node"),
            label(m, "engine"),
            digest.clone(),
            label(m, "collector"),
        );
        let s = by_source.entry(key).or_default();
        if m.name == "db.query.calls" {
            s.calls += m.value
        } else {
            s.total += m.value
        }
    }
    let mut stats = BTreeMap::<(String, String, String), (String, DbStats)>::new();
    for ((node, engine, digest, collector), value) in by_source {
        let key = (node, engine, digest);
        let replace = stats.get(&key).is_none_or(|(current, _)| {
            source_priority("database", &collector) < source_priority("database", current)
        });
        if replace {
            stats.insert(key, (collector, value));
        }
    }
    for ((node, engine, digest), (collector, s)) in stats {
        if valid(s.total) && s.total > 0.0 {
            out.entry("database").or_default().push(Bottleneck {
                category: "database",
                node,
                target: format!("{engine} {digest}"),
                evidence: format!(
                    "total={:.2}ms calls={:.0} mean={:.2}ms",
                    s.total,
                    s.calls,
                    if s.calls > 0.0 {
                        s.total / s.calls
                    } else {
                        0.0
                    }
                ),
                source: source_name("database", &collector),
                severity: s.total,
                verify_metric: "db.query.total_duration",
                strength: "summary-only",
            });
        }
    }
}

fn cpu(metrics: &[Metric], out: &mut BTreeMap<&'static str, Vec<Bottleneck>>) {
    for m in metrics {
        if matches!(m.name.as_str(), "cpu.sample_percent" | "cpu.samples")
            && m.timestamp.is_none()
            && valid(m.value)
            && m.value > 0.0
            && !is_idle_cpu_sample(m)
        {
            out.entry("cpu").or_default().push(Bottleneck {
                category: "cpu",
                node: label(m, "node"),
                target: cpu_target(m),
                evidence: format!("sample_share={:.2}%", m.value),
                source: "perf",
                severity: m.value,
                verify_metric: "cpu.sample_percent",
                strength: "summary-only",
            });
        }
    }
}

fn is_idle_cpu_sample(metric: &Metric) -> bool {
    let process = label(metric, "process");
    process == "swapper"
        || process.starts_with("swapper/")
        || process == "idle"
        || process.starts_with("idle/")
}

fn host(metrics: &[Metric], out: &mut BTreeMap<&'static str, Vec<Bottleneck>>) {
    let mut values =
        BTreeMap::<(String, String, String, String, String), (f64, usize, bool)>::new();
    for m in metrics {
        let (target, verify, divisor) = match m.name.as_str() {
            "host.cpu_percent" => ("cpu".into(), "host.cpu_percent", 100.0),
            "host.disk_util_percent" => (
                format!("disk {} util", label(m, "device")),
                "host.disk_util_percent",
                100.0,
            ),
            "host.disk_await" => (
                format!("disk {} await", label(m, "device")),
                "host.disk_await",
                20.0,
            ),
            _ => continue,
        };
        let collector = label(m, "collector");
        let key = (
            label(m, "node"),
            target,
            m.name.clone(),
            verify.into(),
            collector,
        );
        let entry = values.entry(key).or_default();
        if m.timestamp.is_none() {
            *entry = (m.value / divisor, 1, true);
        } else if !entry.2 {
            entry.0 += m.value / divisor;
            entry.1 += 1;
        }
    }
    let mut selected = BTreeMap::<(String, String), (String, String, String, f64)>::new();
    for ((node, target, metric, verify, collector), (sum, count, _)) in values {
        if count == 0 {
            continue;
        }
        let weight = sum / count as f64;
        let key = (node.clone(), target.clone());
        let replace = selected.get(&key).is_none_or(|(current, _, _, _)| {
            source_priority("host", &collector) < source_priority("host", current)
        });
        if replace {
            selected.insert(key, (collector, metric, verify, weight));
        }
    }
    for ((node, target), (collector, metric, verify, weight)) in selected {
        if valid(weight) && weight > 0.0 {
            let (observed, unit) = match metric.as_str() {
                "host.cpu_percent" | "host.disk_util_percent" => (weight * 100.0, "percent"),
                _ => (weight * 20.0, "ms"),
            };
            out.entry("host").or_default().push(Bottleneck {
                category: "host",
                node,
                target,
                evidence: format!("{}={:.2}{}", metric, observed, unit),
                source: source_name("host", &collector),
                severity: weight,
                verify_metric: match verify.as_str() {
                    "host.cpu_percent" => "host.cpu_percent",
                    "host.disk_util_percent" => "host.disk_util_percent",
                    _ => "host.disk_await",
                },
                strength: "summary-only",
            });
        }
    }
}

fn annotate_strength(candidates: &mut [Bottleneck], metrics: &[Metric]) {
    let hot_host = metrics
        .iter()
        .filter_map(|metric| {
            let at = metric.timestamp?;
            let hot = match metric.name.as_str() {
                "host.cpu_percent" | "host.disk_util_percent" => metric.value >= 80.0,
                "host.disk_await" => metric.value >= 20.0,
                _ => false,
            };
            hot.then(|| (label(metric, "node"), at.timestamp().div_euclid(5) * 5))
        })
        .collect::<BTreeSet<_>>();
    for candidate in candidates {
        if candidate.category == "host" {
            candidate.strength = if metrics.iter().any(|metric| {
                metric.timestamp.is_some()
                    && label(metric, "node") == candidate.node
                    && metric_supports_candidate(metric, candidate)
            }) {
                "direct"
            } else {
                "summary-only"
            };
            continue;
        }
        let buckets = metrics
            .iter()
            .filter_map(|metric| {
                let at = metric.timestamp?;
                (label(metric, "node") == candidate.node
                    && metric_supports_candidate(metric, candidate))
                .then_some(at.timestamp().div_euclid(5) * 5)
            })
            .collect::<BTreeSet<_>>();
        if buckets.is_empty() {
            candidate.strength = "summary-only";
        } else if buckets
            .iter()
            .any(|bucket| hot_host.contains(&(candidate.node.clone(), *bucket)))
        {
            candidate.strength = "corroborated";
        } else {
            candidate.strength = "direct";
        }
    }
}

fn metric_supports_candidate(metric: &Metric, candidate: &Bottleneck) -> bool {
    match candidate.category {
        "http" => {
            matches!(
                metric.name.as_str(),
                "http.requests" | "http.request_duration"
            ) && candidate.target
                == format!("{} {}", label(metric, "method"), label(metric, "route"))
        }
        "database" => {
            matches!(
                metric.name.as_str(),
                "db.query.calls" | "db.query.total_duration"
            ) && candidate.target
                == format!("{} {}", label(metric, "engine"), label(metric, "digest"))
        }
        "cpu" => {
            matches!(
                metric.name.as_str(),
                "cpu.sample_count" | "cpu.sample_percent"
            ) && candidate.target == cpu_target(metric)
        }
        "host" => match metric.name.as_str() {
            "host.cpu_percent" => candidate.target == "cpu",
            "host.disk_util_percent" => {
                candidate.target == format!("disk {} util", label(metric, "device"))
            }
            "host.disk_await" => {
                candidate.target == format!("disk {} await", label(metric, "device"))
            }
            _ => false,
        },
        _ => false,
    }
}

fn cpu_target(metric: &Metric) -> String {
    format!(
        "{} {}",
        crate::collector::canonical_perf_binary(&label(metric, "binary")),
        crate::collector::canonical_perf_symbol(&label(metric, "symbol"))
    )
}

fn source_priority(category: &str, collector: &str) -> u8 {
    match (category, collector) {
        ("http", "alp") => 0,
        ("database", "mysql-log-delta") => 0,
        ("database", value) if value.contains("pg-stat") => 0,
        ("database", "slp") => 1,
        ("host", "host-sampler") => 0,
        ("host", "sysstat") => 1,
        (_, "-") => 2,
        _ => 1,
    }
}

fn source_name(category: &str, collector: &str) -> &'static str {
    match (category, collector) {
        ("http", "alp") => "alp/access-log",
        ("http", "nginx-series" | "user-transition") => "access-log",
        ("http", _) => "http-metric",
        ("database", "mysql-log-delta") => "mysql-slow-log",
        ("database", "slp") => "slp",
        ("database", value) if value.contains("pg-stat") => "pg_stat_statements",
        ("database", _) => "database-metric",
        ("host", "host-sampler") => "host-sampler",
        ("host", "sysstat") => "sysstat",
        ("host", _) => "host-metric",
        _ => "metric",
    }
}

fn coverage_detail(category: &str, collectors: &[CollectorResult]) -> String {
    let relevant = collectors
        .iter()
        .filter(|collector| collector_category(&collector.name) == Some(category))
        .map(|collector| {
            let node = collector.node.as_deref().unwrap_or("local");
            let detail = collector
                .error
                .as_deref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default();
            format!("{}@{}={}{}", collector.name, node, collector.status, detail)
        })
        .collect::<Vec<_>>();
    if relevant.is_empty() {
        "no matching collector recorded".into()
    } else {
        relevant.join(", ")
    }
}

fn collector_category(name: &str) -> Option<&'static str> {
    match name {
        "alp" | "nginx-series" | "user-transition" => Some("http"),
        "slp" | "mysql-log-delta" | "pg-stat-statements" => Some("database"),
        "perf-report" | "perf-series" => Some("cpu"),
        "host-sampler" | "sysstat" | "system-resource-mark" | "system-resource-read" => {
            Some("host")
        }
        _ => None,
    }
}
fn label(m: &Metric, key: &str) -> String {
    m.labels.get(key).cloned().unwrap_or_else(|| "-".into())
}
fn valid(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    fn m(name: &str, value: f64, labels: &[(&str, &str)]) -> Metric {
        Metric {
            name: name.into(),
            value,
            unit: "ms".into(),
            timestamp: None,
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
    #[test]
    fn keeps_each_observed_category_before_filling_top_five() {
        let metrics = vec![
            m("http.requests", 100., &[("route", "/a"), ("method", "GET")]),
            m(
                "http.request_duration",
                10.,
                &[("route", "/a"), ("method", "GET"), ("quantile", "0.95")],
            ),
            m(
                "db.query.calls",
                2.,
                &[("digest", "select ?"), ("engine", "mysql")],
            ),
            m(
                "db.query.total_duration",
                200.,
                &[("digest", "select ?"), ("engine", "mysql")],
            ),
            m(
                "cpu.sample_percent",
                90.,
                &[("symbol", "work"), ("binary", "app")],
            ),
            m("host.disk_await", 30., &[("device", "sda")]),
        ];
        let r = rank(&metrics, &[]);
        assert_eq!(r.candidates.len(), 4);
        assert!(r.coverage.iter().all(|c| c.available));
        assert_eq!(
            r.candidates.iter().map(|c| c.category).collect::<Vec<_>>(),
            vec!["http", "database", "cpu", "host"]
        );
        assert!(
            r.candidates
                .iter()
                .all(|candidate| candidate.strength == "summary-only")
        );
    }

    #[test]
    fn ignores_kernel_idle_task_as_cpu_bottleneck() {
        let metrics = vec![
            m(
                "cpu.sample_percent",
                80.,
                &[
                    ("node", "app1"),
                    ("process", "swapper"),
                    ("binary", "[kernel.kallsyms]"),
                    ("symbol", "[k] native_safe_halt"),
                ],
            ),
            m(
                "cpu.sample_percent",
                12.,
                &[
                    ("node", "app1"),
                    ("process", "nginx"),
                    ("binary", "nginx"),
                    ("symbol", "ngx_http_process_request"),
                ],
            ),
        ];

        let report = rank(&metrics, &[]);

        assert_eq!(report.candidates.len(), 1);
        assert_eq!(
            report.candidates[0].target,
            "nginx ngx_http_process_request"
        );
        assert_eq!(report.candidates[0].evidence, "sample_share=12.00%");
    }

    #[test]
    fn isupipe_practice_run_keeps_cross_source_strength_and_order() {
        let metrics: Vec<Metric> = serde_json::from_str(include_str!(
            "../tests/fixtures/isupipe-practice-bottleneck.json"
        ))
        .unwrap();
        let report = rank(&metrics, &[]);
        let candidates = report
            .candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.category,
                    candidate.node.as_str(),
                    candidate.target.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            candidates,
            vec![
                ("http", "app1", "GET /api/user/:name/icon"),
                (
                    "database",
                    "app1",
                    "mysql select * from livestream_tags where livestream_id = ?",
                ),
                ("cpu", "app1", "mysqld row_search_mvcc"),
                ("host", "app1", "cpu"),
            ]
        );
        assert_eq!(
            report
                .coverage
                .iter()
                .map(|coverage| (coverage.category, coverage.available))
                .collect::<Vec<_>>(),
            vec![
                ("http", true),
                ("database", true),
                ("cpu", true),
                ("host", true),
            ]
        );
        assert_eq!(
            report.candidates[0].evidence,
            "requests=1674 p95=354.00ms impact=592596.00ms"
        );
        assert_eq!(report.candidates[0].strength, "corroborated");
        assert_eq!(report.candidates[1].strength, "corroborated");
        assert_eq!(report.candidates[2].strength, "summary-only");
        assert_eq!(report.candidates[3].strength, "direct");
    }

    #[test]
    fn ignores_timestamped_series_when_aggregate_metrics_exist() {
        let mut aggregate_requests = m(
            "http.requests",
            10.,
            &[("route", "/a"), ("method", "GET"), ("collector", "alp")],
        );
        let aggregate_p95 = m(
            "http.request_duration",
            20.,
            &[
                ("route", "/a"),
                ("method", "GET"),
                ("quantile", "0.95"),
                ("collector", "alp"),
            ],
        );
        let mut bucket_requests = aggregate_requests.clone();
        bucket_requests.value = 7.;
        bucket_requests.timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0);
        bucket_requests
            .labels
            .insert("collector".into(), "nginx-series".into());
        aggregate_requests.timestamp = None;
        let report = rank(&[aggregate_requests, aggregate_p95, bucket_requests], &[]);
        assert_eq!(
            report.candidates[0].evidence,
            "requests=10 p95=20.00ms impact=200.00ms"
        );
    }

    #[test]
    fn time_aligned_host_pressure_corroborates_a_candidate() {
        let mut requests = m(
            "http.requests",
            10.,
            &[
                ("node", "app1"),
                ("route", "/a"),
                ("method", "GET"),
                ("collector", "alp"),
            ],
        );
        let p95 = m(
            "http.request_duration",
            20.,
            &[
                ("node", "app1"),
                ("route", "/a"),
                ("method", "GET"),
                ("quantile", "0.95"),
                ("collector", "alp"),
            ],
        );
        let at = chrono::DateTime::from_timestamp(1_700_000_002, 0);
        let mut request_series = requests.clone();
        request_series.timestamp = at;
        request_series
            .labels
            .insert("collector".into(), "nginx-series".into());
        let mut host_series = m(
            "host.cpu_percent",
            91.,
            &[("node", "app1"), ("collector", "host-sampler")],
        );
        host_series.timestamp = at;
        requests.timestamp = None;

        let report = rank(&[requests, p95, request_series, host_series], &[]);
        let http = report
            .candidates
            .iter()
            .find(|candidate| candidate.category == "http")
            .unwrap();
        assert_eq!(http.strength, "corroborated");
        let host = report
            .candidates
            .iter()
            .find(|candidate| candidate.category == "host")
            .unwrap();
        assert_eq!(host.strength, "direct");
    }

    #[test]
    fn coverage_explains_unavailable_collectors() {
        let collector = CollectorResult {
            name: "perf-series".into(),
            node: Some("app2".into()),
            phase: "after".into(),
            status: "unavailable".into(),
            exit_code: Some(75),
            error: Some("perf_event_paranoid denied sampling".into()),
            log_ids: Vec::new(),
        };
        let report = rank(&[], &[collector]);
        let cpu = report
            .coverage
            .iter()
            .find(|coverage| coverage.category == "cpu")
            .unwrap();
        assert!(!cpu.available);
        assert!(cpu.detail.contains("perf-series@app2=unavailable"));
        assert!(cpu.detail.contains("perf_event_paranoid"));
    }
}
