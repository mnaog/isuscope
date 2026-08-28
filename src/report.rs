use crate::model::{CollectorResult, Metric, RunManifest, Transition};
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;

const COMPACT_LIMIT: usize = 20;

#[derive(Debug, Serialize)]
pub struct RunReport {
    pub schema_version: u32,
    pub run: RunManifest,
    pub coverage: Vec<CoverageSummary>,
    pub http: ReportSection<HttpRouteSummary>,
    pub database: ReportSection<DatabaseSummary>,
    pub cpu: ReportSection<CpuSummary>,
    pub host: ReportSection<HostSummary>,
    pub artifacts: Vec<ProfileArtifact>,
    pub transitions: ReportSection<Transition>,
    pub run_logs: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_logs: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full: Option<FullEvidence>,
}

#[derive(Debug, Serialize)]
pub struct FullEvidence {
    pub summary_metrics: Vec<Metric>,
    pub series_metrics: Vec<Metric>,
    pub transitions: Vec<Transition>,
}

#[derive(Debug, Serialize)]
pub struct ReportSection<T> {
    pub total_count: usize,
    pub truncated: bool,
    pub items: Vec<T>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct CoverageSummary {
    pub section: String,
    pub status: String,
    pub sources: Vec<String>,
    pub missing_metrics: Vec<String>,
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct DatabaseSummary {
    pub node: String,
    pub engine: String,
    pub digest: String,
    pub source: String,
    pub calls: f64,
    pub total_ms: f64,
    pub avg_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub lock_ms: f64,
    pub rows_sent: f64,
    pub rows_examined: f64,
    pub rows_examined_per_call: Option<f64>,
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct CpuSummary {
    pub node: String,
    pub process: String,
    pub binary: String,
    pub symbol: String,
    pub source: String,
    pub sample_percent: f64,
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct HostSummary {
    pub node: String,
    pub metric: String,
    pub target: String,
    pub source: String,
    pub unit: String,
    pub average: f64,
    pub peak: f64,
    pub peak_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ProfileArtifact {
    pub node: String,
    pub kind: String,
    pub status: String,
    pub canonical_path: Option<PathBuf>,
    pub expanded_path: Option<PathBuf>,
    pub error: Option<String>,
}

pub fn build(
    run: RunManifest,
    metrics: Vec<Metric>,
    transitions: Vec<Transition>,
    run_logs: PathBuf,
    latest_logs: Option<PathBuf>,
    include_full: bool,
) -> RunReport {
    let (summary_metrics, series_metrics): (Vec<_>, Vec<_>) = metrics
        .into_iter()
        .partition(|metric| metric.timestamp.is_none());
    let coverage = coverage(&run, &summary_metrics, &series_metrics);
    let http = section(http_routes(&summary_metrics));
    let database = section(database_queries(&summary_metrics));
    let cpu = section(cpu_symbols(&summary_metrics));
    let host = section(host_metrics(&summary_metrics, &series_metrics));
    let artifacts = profile_artifacts(&run.collectors, &run_logs, latest_logs.as_deref());
    let full_transitions = transitions.clone();
    let transitions = section(transitions);
    RunReport {
        schema_version: 3,
        run,
        coverage,
        http,
        database,
        cpu,
        host,
        artifacts,
        transitions,
        run_logs,
        latest_logs,
        full: include_full.then_some(FullEvidence {
            summary_metrics,
            series_metrics,
            transitions: full_transitions,
        }),
    }
}

pub fn write_json(report: &RunReport, writer: impl Write) -> Result<()> {
    serde_json::to_writer_pretty(writer, report)?;
    Ok(())
}

pub fn write_html(report: &RunReport, mut writer: impl Write) -> Result<()> {
    let title = format!("isuscope run {}", short(&report.run.id));
    let score = report
        .run
        .benchmark
        .score
        .map_or_else(|| "-".into(), |score| score.to_string());
    let collectors = report
        .run
        .collectors
        .iter()
        .map(|collector| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape(&collector.status),
                escape(&collector.name),
                escape(collector.node.as_deref().unwrap_or("local")),
                escape(collector.error.as_deref().unwrap_or("")),
            )
        })
        .collect::<String>();
    let routes = report
        .http.items
        .iter()
        .map(|route| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape(&route.method), escape(&route.route), number(route.count),
                optional(route.total_ms), optional(route.avg_ms), optional(route.p95_ms),
                optional(route.p99_ms), percent(route.error_rate),
            )
        })
        .collect::<String>();
    let embedded = serde_json::to_string(report)?.replace('<', "\\u003c");
    write!(
        writer,
        r#"<!doctype html><html lang="ja"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title><style>
:root{{--bg:#0b1020;--panel:#131b2e;--text:#edf2ff;--muted:#9cabc7;--line:#293653;--ok:#65d69e;--bad:#ff7d8b;--accent:#7bb4ff}}*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--text);font:14px/1.5 system-ui,sans-serif}}main{{max-width:1280px;margin:auto;padding:28px}}h1{{font-size:24px;margin:0 0 20px}}h2{{font-size:16px;margin:0 0 12px}}.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(180px,1fr));gap:12px;margin-bottom:20px}}.card,section{{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:16px}}.label{{color:var(--muted);font-size:12px}}.value{{font-size:22px;font-weight:700}}section{{margin:12px 0;overflow:auto}}table{{width:100%;border-collapse:collapse;white-space:nowrap}}th,td{{padding:8px 10px;text-align:right;border-bottom:1px solid var(--line)}}th:first-child,td:first-child,th:nth-child(2),td:nth-child(2){{text-align:left}}th{{color:var(--muted);font-size:12px}}code{{color:var(--accent)}}
</style></head><body><main><h1>{title}</h1><div class="grid"><div class="card"><div class="label">STATE</div><div class="value">{state}</div></div><div class="card"><div class="label">SCORE</div><div class="value">{score}</div></div><div class="card"><div class="label">HTTP ROUTES</div><div class="value">{route_count}</div></div><div class="card"><div class="label">METRICS</div><div class="value">{metric_count}</div></div></div><section><h2>HTTP routes · total time</h2><table><thead><tr><th>METHOD</th><th>ROUTE</th><th>COUNT</th><th>TOTAL ms</th><th>AVG ms</th><th>P95 ms</th><th>P99 ms</th><th>ERROR %</th></tr></thead><tbody>{routes}</tbody></table></section><section><h2>Collectors</h2><table><thead><tr><th>STATUS</th><th>NAME</th><th>NODE</th><th>ERROR</th></tr></thead><tbody>{collectors}</tbody></table></section><p class="label">Raw evidence: <code>{logs}</code></p><script type="application/json" id="isuscope-report">{embedded}</script></main></body></html>"#,
        title = escape(&title),
        state = escape(report.run.state.as_str()),
        score = score,
        route_count = report.http.total_count,
        metric_count = report.run.metric_count,
        routes = routes,
        collectors = collectors,
        logs = escape(
            &report
                .latest_logs
                .as_deref()
                .unwrap_or(&report.run_logs)
                .display()
                .to_string(),
        ),
        embedded = embedded,
    )?;
    Ok(())
}

fn short(id: &str) -> &str {
    id.get(id.len().saturating_sub(8)..).unwrap_or(id)
}
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn optional(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), |value| format!("{value:.2}"))
}
fn number(value: f64) -> String {
    format!("{value:.0}")
}
fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), |value| format!("{:.2}", value * 100.0))
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct HttpRouteSummary {
    pub node: String,
    pub method: String,
    pub route: String,
    pub count: f64,
    pub total_ms: Option<f64>,
    pub avg_ms: Option<f64>,
    pub min_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub max_ms: Option<f64>,
    pub errors: f64,
    pub error_rate: Option<f64>,
    pub response_bytes: Option<f64>,
    pub status_counts: BTreeMap<String, f64>,
}

pub fn http_routes(metrics: &[Metric]) -> Vec<HttpRouteSummary> {
    let alp_routes = metrics
        .iter()
        .filter(|metric| {
            metric.timestamp.is_none()
                && metric.labels.get("collector").map(String::as_str) == Some("alp")
        })
        .filter_map(|metric| {
            Some((
                metric
                    .labels
                    .get("node")
                    .cloned()
                    .unwrap_or_else(|| "-".into()),
                metric.labels.get("method")?.clone(),
                metric.labels.get("route")?.clone(),
            ))
        })
        .collect::<BTreeSet<_>>();
    let mut routes = BTreeMap::<(String, String, String), HttpRouteSummary>::new();
    for metric in metrics.iter().filter(|metric| metric.timestamp.is_none()) {
        let (Some(method), Some(route)) = (metric.labels.get("method"), metric.labels.get("route"))
        else {
            continue;
        };
        let node = metric
            .labels
            .get("node")
            .cloned()
            .unwrap_or_else(|| "-".into());
        let key = (node.clone(), method.clone(), route.clone());
        if alp_routes.contains(&key)
            && metric.labels.get("collector").map(String::as_str) != Some("alp")
            && metric.name != "http.response_bytes"
        {
            continue;
        }
        let summary = routes.entry(key).or_insert_with(|| HttpRouteSummary {
            node,
            method: method.clone(),
            route: route.clone(),
            ..Default::default()
        });
        match metric.name.as_str() {
            "http.requests" => {
                if let Some(class) = metric.labels.get("status_class") {
                    summary.status_counts.insert(class.clone(), metric.value);
                } else {
                    summary.count = metric.value;
                }
            }
            "http.errors" => summary.errors = metric.value,
            "http.response_bytes" => summary.response_bytes = Some(metric.value),
            "http.request_duration_sum" => summary.total_ms = Some(metric.value),
            "http.request_duration_mean" => summary.avg_ms = Some(metric.value),
            "http.request_duration_min" => summary.min_ms = Some(metric.value),
            "http.request_duration_max" => summary.max_ms = Some(metric.value),
            "http.request_duration" => match metric.labels.get("quantile").map(String::as_str) {
                Some("0.50") => summary.p50_ms = Some(metric.value),
                Some("0.95") => summary.p95_ms = Some(metric.value),
                Some("0.99") => summary.p99_ms = Some(metric.value),
                _ => {}
            },
            _ => {}
        }
    }
    for summary in routes.values_mut() {
        summary.error_rate = (summary.count > 0.0).then(|| summary.errors / summary.count);
    }
    let mut routes = routes.into_values().collect::<Vec<_>>();
    routes.sort_by(|a, b| {
        b.total_ms
            .unwrap_or_default()
            .total_cmp(&a.total_ms.unwrap_or_default())
            .then_with(|| b.count.total_cmp(&a.count))
            .then_with(|| a.route.cmp(&b.route))
    });
    routes
}

pub fn database_queries(metrics: &[Metric]) -> Vec<DatabaseSummary> {
    let mut values = BTreeMap::<(String, String, String, String), DatabaseSummary>::new();
    for metric in metrics {
        let Some(digest) = metric.labels.get("digest") else {
            continue;
        };
        let node = label(metric, "node");
        let engine = label(metric, "engine");
        let source = label(metric, "collector");
        let value = values
            .entry((node.clone(), engine.clone(), digest.clone(), source.clone()))
            .or_insert_with(|| DatabaseSummary {
                node,
                engine,
                digest: digest.clone(),
                source,
                ..Default::default()
            });
        match metric.name.as_str() {
            "db.query.calls" => value.calls = metric.value,
            "db.query.total_duration" => value.total_ms = metric.value,
            "db.query.p95_duration" => value.p95_ms = Some(metric.value),
            "db.query.lock_duration" => value.lock_ms = metric.value,
            "db.query.rows_sent" => value.rows_sent = metric.value,
            "db.query.rows_examined" => value.rows_examined = metric.value,
            _ => {}
        }
    }
    for value in values.values_mut() {
        value.avg_ms = divide(value.total_ms, value.calls);
        value.rows_examined_per_call = divide(value.rows_examined, value.calls);
    }
    let mut values = values.into_values().collect::<Vec<_>>();
    values.sort_by(|a, b| {
        b.total_ms
            .total_cmp(&a.total_ms)
            .then_with(|| a.digest.cmp(&b.digest))
    });
    values
}

pub fn cpu_symbols(metrics: &[Metric]) -> Vec<CpuSummary> {
    let mut values = metrics
        .iter()
        .filter(|metric| metric.name == "cpu.sample_percent")
        .map(|metric| CpuSummary {
            node: label(metric, "node"),
            process: label(metric, "process"),
            binary: label(metric, "binary"),
            symbol: label(metric, "symbol"),
            source: label(metric, "collector"),
            sample_percent: metric.value,
        })
        .collect::<Vec<_>>();
    values.sort_by(|a, b| {
        b.sample_percent
            .total_cmp(&a.sample_percent)
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    values
}

pub fn host_metrics(summary: &[Metric], series: &[Metric]) -> Vec<HostSummary> {
    let mut values =
        BTreeMap::<(String, String, String, String), (String, f64, usize, f64, Option<_>)>::new();
    let series_keys = series
        .iter()
        .filter(|metric| is_host_metric(metric))
        .map(host_key)
        .collect::<BTreeSet<_>>();
    for metric in series.iter().filter(|metric| is_host_metric(metric)).chain(
        summary
            .iter()
            .filter(|metric| is_host_metric(metric) && !series_keys.contains(&host_key(metric))),
    ) {
        let target = metric
            .labels
            .get("device")
            .cloned()
            .unwrap_or_else(|| "host".into());
        let entry = values
            .entry((
                label(metric, "node"),
                metric.name.clone(),
                target,
                label(metric, "collector"),
            ))
            .or_insert_with(|| (metric.unit.clone(), 0.0, 0, metric.value, metric.timestamp));
        entry.1 += metric.value;
        entry.2 += 1;
        if metric.value > entry.3 {
            entry.3 = metric.value;
            entry.4 = metric.timestamp;
        }
    }
    let mut output = values
        .into_iter()
        .map(
            |((node, metric, target, source), (unit, sum, count, peak, peak_at))| HostSummary {
                node,
                metric,
                target,
                source,
                unit,
                average: sum / count as f64,
                peak,
                peak_at,
            },
        )
        .collect::<Vec<_>>();
    output.sort_by(|a, b| a.node.cmp(&b.node).then_with(|| a.metric.cmp(&b.metric)));
    output
}

fn profile_artifacts(
    collectors: &[CollectorResult],
    run_logs: &std::path::Path,
    latest_logs: Option<&std::path::Path>,
) -> Vec<ProfileArtifact> {
    collectors
        .iter()
        .filter_map(|collector| {
            let (kind, extension) = match collector.name.as_str() {
                "perf-flamegraph" => ("cpu-flamegraph", "svg"),
                "offcpu" => ("offcpu-folded", "folded"),
                _ => return None,
            };
            let log_id = collector.log_ids.first()?;
            Some(ProfileArtifact {
                node: collector.node.clone().unwrap_or_else(|| "local".into()),
                kind: kind.into(),
                status: collector.status.clone(),
                canonical_path: (collector.status == "complete")
                    .then(|| run_logs.join(format!("{log_id}.zst"))),
                expanded_path: latest_logs
                    .filter(|_| collector.status == "complete")
                    .map(|logs| logs.join(format!("{log_id}.{extension}"))),
                error: collector.error.clone(),
            })
        })
        .collect()
}

fn section<T>(mut items: Vec<T>) -> ReportSection<T> {
    let total_count = items.len();
    items.truncate(COMPACT_LIMIT);
    ReportSection {
        total_count,
        truncated: total_count > items.len(),
        items,
    }
}

fn coverage(run: &RunManifest, summary: &[Metric], series: &[Metric]) -> Vec<CoverageSummary> {
    ["http", "database", "cpu", "host", "profiles"]
        .into_iter()
        .map(|section| {
            let collectors = run
                .collectors
                .iter()
                .filter(|collector| collector_section(&collector.name) == Some(section))
                .collect::<Vec<_>>();
            let status = if collectors
                .iter()
                .any(|collector| collector.status == "complete")
            {
                "complete"
            } else if collectors
                .iter()
                .any(|collector| collector.status == "failed")
            {
                "failed"
            } else {
                "unavailable"
            };
            CoverageSummary {
                section: section.into(),
                status: status.into(),
                sources: collectors
                    .iter()
                    .map(|collector| {
                        format!(
                            "{}@{}={}",
                            collector.name,
                            collector.node.as_deref().unwrap_or("local"),
                            collector.status
                        )
                    })
                    .collect(),
                missing_metrics: expected_metrics(section)
                    .iter()
                    .filter(|name| {
                        !summary
                            .iter()
                            .chain(series)
                            .any(|metric| metric.name == **name)
                    })
                    .map(|name| name.to_string())
                    .collect(),
            }
        })
        .collect()
}

fn collector_section(name: &str) -> Option<&'static str> {
    match name {
        "alp" | "nginx-series" | "user-transition" => Some("http"),
        "slp" | "mysql-log-delta" | "pg-stat-statements" => Some("database"),
        "perf-report" | "perf-series" => Some("cpu"),
        "host-sampler" | "sysstat" => Some("host"),
        "perf-flamegraph" | "offcpu" => Some("profiles"),
        _ => None,
    }
}

fn expected_metrics(section: &str) -> &'static [&'static str] {
    match section {
        "http" => &["http.requests", "http.request_duration"],
        "database" => &["db.query.calls", "db.query.total_duration"],
        "cpu" => &["cpu.sample_percent"],
        "host" => &["host.cpu_percent", "host.memory_used_bytes"],
        _ => &[],
    }
}

fn host_key(metric: &Metric) -> (String, String, String) {
    (
        label(metric, "node"),
        metric.name.clone(),
        metric
            .labels
            .get("device")
            .cloned()
            .unwrap_or_else(|| "host".into()),
    )
}

fn label(metric: &Metric, name: &str) -> String {
    metric
        .labels
        .get(name)
        .cloned()
        .unwrap_or_else(|| "-".into())
}
fn divide(numerator: f64, denominator: f64) -> Option<f64> {
    (denominator > 0.0).then(|| numerator / denominator)
}
fn is_host_metric(metric: &Metric) -> bool {
    metric.name.starts_with("host.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_http_metrics_without_inventing_cross_route_scores() {
        let labels = BTreeMap::from([
            ("method".into(), "GET".into()),
            ("route".into(), "/items/:id".into()),
        ]);
        let metric = |name: &str, value: f64, labels: BTreeMap<String, String>| Metric {
            name: name.into(),
            value,
            unit: String::new(),
            timestamp: None,
            labels,
        };
        let metrics = vec![
            metric("http.requests", 10.0, labels.clone()),
            metric("http.request_duration_sum", 80.0, labels.clone()),
            metric("http.errors", 2.0, labels.clone()),
        ];
        let report = http_routes(&metrics);
        assert_eq!(report[0].total_ms, Some(80.0));
        assert_eq!(report[0].error_rate, Some(0.2));
    }

    #[test]
    fn summarizes_database_cpu_and_host_evidence() {
        let metric = |name: &str, value: f64, labels: &[(&str, &str)]| Metric {
            name: name.into(),
            value,
            unit: if name.starts_with("host.") {
                "percent"
            } else {
                "ms"
            }
            .into(),
            timestamp: None,
            labels: labels
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        };
        let db_labels = [("node", "db1"), ("engine", "mysql"), ("digest", "select ?")];
        let database = database_queries(&[
            metric("db.query.calls", 4.0, &db_labels),
            metric("db.query.total_duration", 80.0, &db_labels),
            metric("db.query.rows_examined", 100.0, &db_labels),
        ]);
        assert_eq!(database[0].avg_ms, Some(20.0));
        assert_eq!(database[0].rows_examined_per_call, Some(25.0));

        let cpu = cpu_symbols(&[metric(
            "cpu.sample_percent",
            42.0,
            &[
                ("node", "app1"),
                ("process", "app"),
                ("binary", "web"),
                ("symbol", "work"),
            ],
        )]);
        assert_eq!(cpu[0].sample_percent, 42.0);

        let mut first = metric("host.cpu_percent", 20.0, &[("node", "app1")]);
        first.timestamp = "2026-08-27T12:00:00Z".parse().ok();
        let mut second = metric("host.cpu_percent", 80.0, &[("node", "app1")]);
        second.timestamp = "2026-08-27T12:00:01Z".parse().ok();
        let host = host_metrics(&[], &[first, second]);
        assert_eq!(host[0].average, 50.0);
        assert_eq!(host[0].peak, 80.0);
        assert!(host[0].peak_at.is_some());

        let summary_only = metric("host.cpu_percent", 30.0, &[("node", "db1")]);
        let mut series_only = metric("host.cpu_percent", 70.0, &[("node", "app1")]);
        series_only.timestamp = "2026-08-27T12:00:02Z".parse().ok();
        let mixed = host_metrics(&[summary_only], &[series_only]);
        assert_eq!(mixed.len(), 2);
        assert!(
            mixed
                .iter()
                .any(|value| value.node == "db1" && value.peak == 30.0)
        );
    }

    #[test]
    fn compact_sections_expose_total_and_truncation() {
        let values = (0..25).collect::<Vec<_>>();
        let compact = section(values);
        assert_eq!(compact.total_count, 25);
        assert_eq!(compact.items.len(), COMPACT_LIMIT);
        assert!(compact.truncated);
    }

    #[test]
    fn artifacts_always_reference_the_run_and_only_latest_gets_expanded_paths() {
        let collector = CollectorResult {
            name: "perf-flamegraph".into(),
            node: Some("app1".into()),
            phase: "after".into(),
            status: "complete".into(),
            exit_code: Some(0),
            error: None,
            log_ids: vec!["perf-flamegraph-app1-after-stdout".into()],
        };
        let run_logs = std::path::Path::new("runs/old/logs");
        let historical = profile_artifacts(std::slice::from_ref(&collector), run_logs, None);
        assert_eq!(
            historical[0].canonical_path.as_deref(),
            Some(std::path::Path::new(
                "runs/old/logs/perf-flamegraph-app1-after-stdout.zst"
            ))
        );
        assert_eq!(historical[0].expanded_path, None);

        let latest = profile_artifacts(
            &[collector],
            run_logs,
            Some(std::path::Path::new("latest/logs")),
        );
        assert_eq!(
            latest[0].expanded_path.as_deref(),
            Some(std::path::Path::new(
                "latest/logs/perf-flamegraph-app1-after-stdout.svg"
            ))
        );
    }
}
