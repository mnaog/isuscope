use crate::model::{CollectorResult, Metric, RunManifest, Transition};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Lossless normalized data used by the brief output.
/// This is intentionally not serializable: public output must choose its own
/// ordering and compaction only after all relevant records have been compared.
pub struct RunDiagnostics {
    pub run: RunManifest,
    pub coverage: Vec<CoverageSummary>,
    pub http: Vec<HttpRouteSummary>,
    pub database: Vec<DatabaseSummary>,
    pub cpu: Vec<CpuSummary>,
    pub host: Vec<HostSummary>,
    /// Load-generator side: connections held and opened, and the wait between requests.
    pub client: Vec<HostSummary>,
    /// hostとclientの時系列をどの区間で要約したか。initializeの終わりが分かるrunは`load`。
    pub host_window: &'static str,
    pub upstreams: Vec<UpstreamSummary>,
    pub artifacts: Vec<ProfileArtifact>,
    pub transitions: Vec<Transition>,
    pub transition_order: TransitionOrder,
}

/// 遷移helperが残した順序の確からしさ。遷移（from, to）ごとの`transition.overlap_count`と
/// `transition.ambiguous_count`、開始時刻を推定できなかった要求の数（`transition.legacy_events`）。
/// helperが順序を記録する前のrunでは空。
#[derive(Debug, Default)]
pub struct TransitionOrder {
    pub edges: BTreeMap<(String, String), TransitionEdgeOrder>,
    pub legacy_events: Option<f64>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TransitionEdgeOrder {
    pub overlap_count: Option<f64>,
    pub ambiguous_count: Option<f64>,
}

fn transition_order(metrics: &[Metric]) -> TransitionOrder {
    let mut order = TransitionOrder::default();
    for metric in metrics {
        let add = |slot: &mut Option<f64>| *slot.get_or_insert(0.0) += metric.value;
        match metric.name.as_str() {
            "transition.legacy_events" => add(&mut order.legacy_events),
            name @ ("transition.overlap_count" | "transition.ambiguous_count") => {
                let (Some(from), Some(to)) = (metric.labels.get("from"), metric.labels.get("to"))
                else {
                    continue;
                };
                let edge = order.edges.entry((from.clone(), to.clone())).or_default();
                add(if name == "transition.overlap_count" {
                    &mut edge.overlap_count
                } else {
                    &mut edge.ambiguous_count
                });
            }
            _ => {}
        }
    }
    order
}

#[derive(Debug, Serialize, PartialEq)]
pub struct CoverageSummary {
    pub section: String,
    pub node: String,
    pub collector: String,
    pub phase: String,
    pub status: String,
    pub missing_metrics: Vec<String>,
    /// collectorが失敗したときのerror。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct DatabaseSummary {
    pub node: String,
    pub engine: String,
    #[serde(serialize_with = "crate::query::serialize_capped")]
    pub digest: String,
    /// `digest`を長さの上限で切ったときだけ付く、切る前の全文のhash。集計と照合の識別に使う。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest_id: Option<String>,
    pub source: String,
    /// node上で区間ごとに集計したsource（slp）の区間。`initialize`、`load`、`whole`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    pub calls: f64,
    pub total_ms: f64,
    pub avg_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p99_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<f64>,
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

/// One upstream (backend) as seen by the edge: how many requests it served, how many were
/// retried, and where their time went (connecting, generating headers, sending the response).
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct UpstreamSummary {
    pub node: String,
    pub upstream: String,
    pub requests: f64,
    pub retried_requests: f64,
    pub connect_p95_ms: Option<f64>,
    pub header_p95_ms: Option<f64>,
    pub response_avg_ms: Option<f64>,
    pub response_p95_ms: Option<f64>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct HostSummary {
    pub node: String,
    pub metric: String,
    pub target: String,
    pub source: String,
    /// coreやquantileなど、同じ名前の中で対象を分けるlabel。混ぜると値の意味が壊れるため、
    /// 集約keyに含めてそのまま残します。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    pub unit: String,
    /// `value`の作り方。分位点のように再集約できないものは`non-mergeable`で`value`はnullです。
    pub aggregation: crate::metric_semantics::MetricAggregation,
    pub value: Option<f64>,
    pub peak: f64,
    pub peak_at: Option<chrono::DateTime<chrono::Utc>>,
    pub samples: usize,
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

pub fn diagnose(
    run: RunManifest,
    metrics: Vec<Metric>,
    transitions: Vec<Transition>,
    run_logs: PathBuf,
    latest_logs: Option<PathBuf>,
) -> RunDiagnostics {
    let (summary_metrics, series_metrics): (Vec<_>, Vec<_>) = metrics
        .into_iter()
        .partition(|metric| metric.timestamp.is_none());
    let coverage = coverage(&run.collectors, &summary_metrics, &series_metrics);
    let http = http_routes(&summary_metrics);
    let database = database_queries(&summary_metrics);
    let cpu = cpu_symbols(&summary_metrics);
    // initializeは負荷走行と別の負荷なので、hostとclientの要約には混ぜない。
    // initializeの終わりが分からないrunだけ、全区間で要約する。
    let (host_window, windowed_series) = match load_window(&run) {
        Some((start, end)) => (
            "load",
            series_metrics
                .iter()
                .filter(|metric| {
                    metric
                        .timestamp
                        .is_some_and(|at| crate::model::in_window(at, start, end))
                })
                .cloned()
                .collect::<Vec<_>>(),
        ),
        None => ("whole", series_metrics.clone()),
    };
    let host = host_metrics(&summary_metrics, &windowed_series);
    let client = client_metrics(&summary_metrics, &windowed_series);
    let mut upstreams = upstream_summaries(&summary_metrics);
    upstreams.sort_by(|a, b| {
        b.requests
            .total_cmp(&a.requests)
            .then_with(|| a.upstream.cmp(&b.upstream))
    });
    let artifacts = profile_artifacts(&run.collectors, &run_logs, latest_logs.as_deref());
    let transition_order = transition_order(&summary_metrics);
    RunDiagnostics {
        run,
        coverage,
        http,
        database,
        cpu,
        host,
        client,
        host_window,
        upstreams,
        artifacts,
        transitions,
        transition_order,
    }
}

/// 負荷走行の区間。initializeの終わりからベンチの終わりまで。
fn load_window(
    run: &RunManifest,
) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    Some((
        run.benchmark.initialize_finished_at?,
        run.benchmark.finished_at?,
    ))
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
    /// 0件のclassは出さない。どのclassも0件なら欄ごと出さない。
    #[serde(serialize_with = "nonzero_counts", skip_serializing_if = "all_zero")]
    pub status_counts: BTreeMap<String, f64>,
}

fn all_zero(counts: &BTreeMap<String, f64>) -> bool {
    counts.values().all(|count| *count == 0.0)
}

fn nonzero_counts<S: serde::Serializer>(
    counts: &BTreeMap<String, f64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_map(counts.iter().filter(|(_, count)| **count != 0.0))
}

pub fn http_routes(metrics: &[Metric]) -> Vec<HttpRouteSummary> {
    type SourceKey = (String, String, String, String);
    let mut sources = BTreeMap::<SourceKey, HttpRouteSummary>::new();
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
        let source = label(metric, "collector");
        let summary = sources
            .entry((node.clone(), method.clone(), route.clone(), source))
            .or_insert_with(|| HttpRouteSummary {
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

    for summary in sources.values_mut() {
        if summary.count == 0.0 && !summary.status_counts.is_empty() {
            summary.count = summary.status_counts.values().sum();
        }
    }
    let mut grouped = BTreeMap::<(String, String, String), Vec<HttpRouteSummary>>::new();
    for ((node, method, route, _), summary) in sources {
        grouped
            .entry((node, method, route))
            .or_default()
            .push(summary);
    }
    let mut routes = Vec::with_capacity(grouped.len());
    for (_, mut candidates) in grouped {
        candidates.sort_by_key(|candidate| std::cmp::Reverse(http_source_quality(candidate)));
        let mut selected = candidates.remove(0);
        for fallback in candidates {
            if selected.count == 0.0 {
                selected.count = fallback.count;
            }
            selected.total_ms = selected.total_ms.or(fallback.total_ms);
            selected.avg_ms = selected.avg_ms.or(fallback.avg_ms);
            selected.min_ms = selected.min_ms.or(fallback.min_ms);
            selected.p50_ms = selected.p50_ms.or(fallback.p50_ms);
            selected.p95_ms = selected.p95_ms.or(fallback.p95_ms);
            selected.p99_ms = selected.p99_ms.or(fallback.p99_ms);
            selected.max_ms = selected.max_ms.or(fallback.max_ms);
            selected.response_bytes = selected.response_bytes.or(fallback.response_bytes);
            if selected.status_counts.is_empty() {
                selected.status_counts = fallback.status_counts;
                selected.errors = fallback.errors;
            }
        }
        if selected.total_ms.is_none() {
            selected.total_ms = selected.avg_ms.map(|average| average * selected.count);
        }
        if selected.avg_ms.is_none() {
            selected.avg_ms = selected
                .total_ms
                .and_then(|total| divide(total, selected.count));
        }
        selected.error_rate = (selected.count > 0.0).then(|| selected.errors / selected.count);
        routes.push(selected);
    }
    routes.sort_by(|a, b| {
        b.total_ms
            .unwrap_or_default()
            .total_cmp(&a.total_ms.unwrap_or_default())
            .then_with(|| b.count.total_cmp(&a.count))
            .then_with(|| a.route.cmp(&b.route))
    });
    routes
}

fn http_source_quality(summary: &HttpRouteSummary) -> (bool, bool, bool, bool, usize) {
    (
        summary.total_ms.is_some(),
        summary.count > 0.0,
        summary.avg_ms.is_some(),
        summary.p95_ms.is_some(),
        summary.status_counts.len(),
    )
}

pub fn database_queries(metrics: &[Metric]) -> Vec<DatabaseSummary> {
    type Key = (
        String,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
    );
    let mut values = BTreeMap::<Key, DatabaseSummary>::new();
    // 同じキーへ2回目の集計が来た行。`digest_id`の無い過去のrunで、切った文が衝突したもの。
    let mut merged = BTreeSet::<Key>::new();
    for metric in metrics {
        let Some(digest) = metric.labels.get("digest") else {
            continue;
        };
        let node = label(metric, "node");
        let engine = label(metric, "engine");
        let source = label(metric, "collector");
        let window = metric.labels.get("window").cloned();
        let digest_id = metric.labels.get("digest_id").cloned();
        let key = (
            node.clone(),
            engine.clone(),
            digest.clone(),
            digest_id.clone(),
            source.clone(),
            window.clone(),
        );
        if metric.name == "db.query.calls"
            && values.get(&key).is_some_and(|value| value.calls > 0.0)
        {
            merged.insert(key.clone());
        }
        let value = values.entry(key).or_insert_with(|| DatabaseSummary {
            node,
            engine,
            digest: digest.clone(),
            digest_id,
            source,
            window,
            ..Default::default()
        });
        // 回数と時間は足す。上書きすると、衝突した別の文の分が消える。
        match metric.name.as_str() {
            "db.query.calls" => value.calls += metric.value,
            "db.query.total_duration" => value.total_ms += metric.value,
            "db.query.p95_duration" => value.p95_ms = Some(metric.value),
            "db.query.p99_duration" => value.p99_ms = Some(metric.value),
            "db.query.duration_max" => {
                value.max_ms = Some(value.max_ms.unwrap_or(0.0).max(metric.value))
            }
            "db.query.lock_duration" => value.lock_ms += metric.value,
            "db.query.rows_sent" => value.rows_sent += metric.value,
            "db.query.rows_examined" => value.rows_examined += metric.value,
            _ => {}
        }
    }
    for (key, value) in &mut values {
        value.avg_ms = divide(value.total_ms, value.calls);
        value.rows_examined_per_call = divide(value.rows_examined, value.calls);
        // 別々の文の分位点は合わせられない。
        if merged.contains(key) {
            value.p95_ms = None;
            value.p99_ms = None;
        }
    }
    let mut values = values.into_values().collect::<Vec<_>>();
    values.sort_by(|a, b| {
        b.total_ms
            .total_cmp(&a.total_ms)
            .then_with(|| a.digest.cmp(&b.digest))
            .then_with(|| a.digest_id.cmp(&b.digest_id))
    });
    values
}

/// Whether a run used the window-aware database collector. Successful empty captures carry no
/// metric rows, so row presence alone cannot distinguish them from legacy runs.
pub fn supports_database_windows(run: &RunManifest, metrics: &[Metric]) -> bool {
    metrics
        .iter()
        .any(|metric| metric.labels.contains_key("window"))
        || (metrics.is_empty()
            && run
                .collectors
                .iter()
                .any(|collector| collector.name == "slp" && collector.status == "complete"))
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

pub fn upstream_summaries(summary: &[Metric]) -> Vec<UpstreamSummary> {
    let mut rows = BTreeMap::<(String, String), UpstreamSummary>::new();
    for metric in summary.iter().filter(|metric| metric.timestamp.is_none()) {
        let Some(upstream) = metric.labels.get("upstream") else {
            continue;
        };
        let node = label(metric, "node");
        let row = rows
            .entry((node.clone(), upstream.clone()))
            .or_insert_with(|| UpstreamSummary {
                node,
                upstream: upstream.clone(),
                ..Default::default()
            });
        let quantile = metric.labels.get("quantile").map(String::as_str);
        match (metric.name.as_str(), quantile) {
            ("http.upstream_requests", _) => row.requests = metric.value,
            ("http.upstream_retried_requests", _) => row.retried_requests = metric.value,
            ("http.upstream_connect_duration", Some("0.95")) => {
                row.connect_p95_ms = Some(metric.value)
            }
            ("http.upstream_header_duration", Some("0.95")) => {
                row.header_p95_ms = Some(metric.value)
            }
            ("http.upstream_response_duration", Some("0.95")) => {
                row.response_p95_ms = Some(metric.value)
            }
            ("http.upstream_response_duration_mean", _) => row.response_avg_ms = Some(metric.value),
            _ => {}
        }
    }
    rows.into_values().collect()
}

pub fn host_metrics(summary: &[Metric], series: &[Metric]) -> Vec<HostSummary> {
    summarize_metrics(summary, series, is_host_metric)
}

/// Shared by host and client summaries: average and peak per node, metric and target.
fn summarize_metrics(
    summary: &[Metric],
    series: &[Metric],
    keep: fn(&Metric) -> bool,
) -> Vec<HostSummary> {
    struct Group {
        unit: String,
        aggregation: crate::metric_semantics::MetricAggregation,
        values: Vec<f64>,
        peak: f64,
        peak_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let mut values = BTreeMap::<HostKey, Group>::new();
    let series_keys = series
        .iter()
        .filter(|metric| keep(metric))
        .map(host_key)
        .collect::<BTreeSet<_>>();
    for metric in series.iter().filter(|metric| keep(metric)).chain(
        summary
            .iter()
            .filter(|metric| keep(metric) && !series_keys.contains(&host_key(metric))),
    ) {
        let entry = values.entry(host_key(metric)).or_insert_with(|| Group {
            unit: metric.unit.clone(),
            aggregation: crate::metric_semantics::time_series_aggregation(metric),
            values: Vec::new(),
            peak: metric.value,
            peak_at: metric.timestamp,
        });
        entry.values.push(metric.value);
        if metric.value > entry.peak {
            entry.peak = metric.value;
            entry.peak_at = metric.timestamp;
        }
    }
    let mut output = values
        .into_iter()
        .map(
            |((node, metric, target, source, labels), group)| HostSummary {
                node,
                metric,
                target,
                source,
                labels: labels.into_iter().collect(),
                unit: group.unit,
                aggregation: group.aggregation,
                value: crate::metric_semantics::aggregate(&group.values, group.aggregation),
                peak: group.peak,
                peak_at: group.peak_at,
                samples: group.values.len(),
            },
        )
        .collect::<Vec<_>>();
    output.sort_by(|a, b| {
        a.node
            .cmp(&b.node)
            .then_with(|| a.metric.cmp(&b.metric))
            .then_with(|| a.labels.cmp(&b.labels))
            .then_with(|| a.target.cmp(&b.target))
    });
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

fn coverage(
    collectors: &[CollectorResult],
    summary: &[Metric],
    series: &[Metric],
) -> Vec<CoverageSummary> {
    let metrics = summary.iter().chain(series).collect::<Vec<_>>();
    let mut coverage = Vec::new();
    for section in ["http", "database", "cpu", "host", "profiles"] {
        let section_collectors = collectors
            .iter()
            .filter(|collector| collector_section(&collector.name) == Some(section))
            .collect::<Vec<_>>();
        if section_collectors.is_empty() {
            coverage.push(CoverageSummary {
                section: section.into(),
                node: "-".into(),
                collector: "-".into(),
                phase: "-".into(),
                status: "missing".into(),
                missing_metrics: expected_metrics(section)
                    .iter()
                    .map(|name| name.to_string())
                    .collect(),
                error: None,
            });
            continue;
        }
        for collector in section_collectors {
            let missing_metrics = expected_collector_metrics(&collector.name)
                .iter()
                .filter(|name| {
                    !metrics.iter().any(|metric| {
                        metric.name == **name && metric_matches_collector(metric, collector)
                    })
                })
                .map(|name| name.to_string())
                .collect::<Vec<_>>();
            let unclassified: f64 = metrics
                .iter()
                .filter(|metric| {
                    metric.name == "db.slow_log_unclassified"
                        && metric_matches_collector(metric, collector)
                })
                .map(|metric| metric.value)
                .sum();
            let status = match collector.status.as_str() {
                "complete" if unclassified > 0.0 => "partial",
                "complete" if missing_metrics.is_empty() => "complete",
                "complete" => "missing",
                "unavailable" => "unavailable",
                "failed" => "failed",
                _ => "missing",
            };
            coverage.push(CoverageSummary {
                section: section.into(),
                node: collector.node.clone().unwrap_or_else(|| "local".into()),
                collector: collector.name.clone(),
                phase: collector.phase.clone(),
                status: status.into(),
                missing_metrics,
                error: collector.error.clone().or_else(|| (unclassified > 0.0).then(||
                    format!("{unclassified} slow-log records could not be assigned to a window; database aggregation is incomplete")
                )),
            });
        }
    }
    // HTTP・DB・CPU・hostのどれにも分類していないcollector（mysql-status、service-throttle、
    // 後から足したcollectorなど）も、失敗はrunをdegradedにする。分類の一覧に無くても、失敗と
    // そのerrorは必ず見えるようにする。
    for collector in collectors
        .iter()
        .filter(|collector| collector_section(&collector.name).is_none())
        .filter(|collector| collector.status == "failed")
    {
        coverage.push(CoverageSummary {
            section: "other".into(),
            node: collector.node.clone().unwrap_or_else(|| "local".into()),
            collector: collector.name.clone(),
            phase: collector.phase.clone(),
            status: collector.status.clone(),
            missing_metrics: Vec::new(),
            error: collector.error.clone(),
        });
    }
    coverage
}

fn collector_section(name: &str) -> Option<&'static str> {
    match name {
        "alp" | "nginx-series" | "user-transition" => Some("http"),
        "slp" | "mysql-log-delta" | "pg-stat-statements" => Some("database"),
        "perf-report" | "perf-series" => Some("cpu"),
        "host-sampler" | "sysstat" | "service-sampler" => Some("host"),
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

fn expected_collector_metrics(collector: &str) -> &'static [&'static str] {
    match collector {
        "alp" | "nginx-series" => &["http.requests", "http.request_duration"],
        // mysql-log-deltaはslow logの差分をnode上に用意するだけで、集計はslpが行う。
        "slp" | "pg-stat-statements" => &["db.query.calls", "db.query.total_duration"],
        "perf-report" | "perf-series" => &["cpu.sample_percent"],
        "host-sampler" => &["host.cpu_percent", "host.memory_used_bytes"],
        "sysstat" => &["host.cpu_percent"],
        "service-sampler" => &["service.cpu_cores", "service.memory_bytes"],
        _ => &[],
    }
}

fn metric_matches_collector(metric: &Metric, collector: &CollectorResult) -> bool {
    if metric.labels.get("collector").map(String::as_str) != Some(collector.name.as_str()) {
        return false;
    }
    match collector.node.as_deref() {
        Some(node) => metric.labels.get("node").map(String::as_str) == Some(node),
        // localのcollector（nginx-seriesなど）は、nodeから持ち帰ったlogをnode別に集計するので、
        // metricにはそのnodeの名前が付く。collector名が一致すれば、そのcollectorの出力である。
        None => true,
    }
}

/// node、metric名、対象（device/service）、collector、そして残りのlabel。
/// 最後の要素を落とすと、coreやquantileの違う値が1行に混ざります。
type HostKey = (String, String, String, String, Vec<(String, String)>);

fn host_key(metric: &Metric) -> HostKey {
    (
        label(metric, "node"),
        metric.name.clone(),
        metric
            .labels
            .get("device")
            .or_else(|| metric.labels.get("service"))
            .cloned()
            .unwrap_or_else(|| "host".into()),
        label(metric, "collector"),
        metric
            .labels
            .iter()
            .filter(|(name, _)| {
                !matches!(name.as_str(), "node" | "collector" | "device" | "service")
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
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
        || metric.name.starts_with("service.")
        || metric.name.starts_with("mysql.")
}

/// How the load generator drove the system: connections it held and opened, requests per
/// connection, and the gap between a response and that connection's next request.
fn is_client_metric(metric: &Metric) -> bool {
    metric.name.starts_with("client.")
}

pub fn client_metrics(summary: &[Metric], series: &[Metric]) -> Vec<HostSummary> {
    summarize_metrics(summary, series, is_client_metric)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_rows_keep_requests_retries_and_p95_per_backend() {
        let labels = |upstream: &str, extra: &[(&str, &str)]| {
            let mut labels = BTreeMap::from([
                ("node".to_string(), "edge".to_string()),
                ("upstream".to_string(), upstream.to_string()),
            ]);
            labels.extend(
                extra
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string())),
            );
            labels
        };
        let metric = |name: &str, value: f64, labels: BTreeMap<String, String>| Metric {
            name: name.into(),
            value,
            unit: "ms".into(),
            timestamp: None,
            labels,
        };
        let rows = upstream_summaries(&[
            metric(
                "http.upstream_requests",
                120.0,
                labels("10.0.0.2:8080", &[]),
            ),
            metric(
                "http.upstream_retried_requests",
                3.0,
                labels("10.0.0.2:8080", &[]),
            ),
            metric(
                "http.upstream_connect_duration",
                2.5,
                labels("10.0.0.2:8080", &[("quantile", "0.95")]),
            ),
            metric(
                "http.upstream_response_duration",
                40.0,
                labels("10.0.0.2:8080", &[("quantile", "0.95")]),
            ),
            metric(
                "http.upstream_response_duration_mean",
                12.0,
                labels("10.0.0.2:8080", &[]),
            ),
            metric("http.upstream_requests", 80.0, labels("10.0.0.3:8080", &[])),
            // A per-route metric without an upstream label stays out of these rows.
            metric(
                "http.requests",
                200.0,
                BTreeMap::from([("route".to_string(), "/items".to_string())]),
            ),
        ]);
        assert_eq!(
            rows,
            vec![
                UpstreamSummary {
                    node: "edge".into(),
                    upstream: "10.0.0.2:8080".into(),
                    requests: 120.0,
                    retried_requests: 3.0,
                    connect_p95_ms: Some(2.5),
                    header_p95_ms: None,
                    response_avg_ms: Some(12.0),
                    response_p95_ms: Some(40.0),
                },
                UpstreamSummary {
                    node: "edge".into(),
                    upstream: "10.0.0.3:8080".into(),
                    requests: 80.0,
                    ..Default::default()
                },
            ]
        );
    }

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
    fn http_summary_derives_count_from_status_classes() {
        let base = BTreeMap::from([
            ("collector".into(), "user-transition".into()),
            ("node".into(), "app1".into()),
            ("method".into(), "GET".into()),
            ("route".into(), "/items".into()),
        ]);
        let metric = |name: &str, value: f64, labels: BTreeMap<String, String>| Metric {
            name: name.into(),
            value,
            unit: String::new(),
            timestamp: None,
            labels,
        };
        let mut success = base.clone();
        success.insert("status_class".into(), "2xx".into());
        let mut failure = base.clone();
        failure.insert("status_class".into(), "5xx".into());
        let report = http_routes(&[
            metric("http.requests", 8.0, success),
            metric("http.requests", 2.0, failure),
            metric("http.errors", 2.0, base),
        ]);
        assert_eq!(report[0].count, 10.0);
        assert_eq!(report[0].error_rate, Some(0.2));
    }

    #[test]
    fn http_summary_prefers_complete_source_and_fills_optional_fields() {
        let labels = |collector: &str| {
            BTreeMap::from([
                ("collector".into(), collector.into()),
                ("node".into(), "app1".into()),
                ("method".into(), "GET".into()),
                ("route".into(), "/items".into()),
            ])
        };
        let metric = |name: &str, value: f64, labels: BTreeMap<String, String>| Metric {
            name: name.into(),
            value,
            unit: String::new(),
            timestamp: None,
            labels,
        };
        let report = http_routes(&[
            metric("http.requests", 10.0, labels("custom-access-summary")),
            metric(
                "http.request_duration_sum",
                80.0,
                labels("custom-access-summary"),
            ),
            metric(
                "http.request_duration_mean",
                8.0,
                labels("custom-access-summary"),
            ),
            metric("http.response_bytes", 1234.0, labels("user-transition")),
        ]);
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].count, 10.0);
        assert_eq!(report[0].total_ms, Some(80.0));
        assert_eq!(report[0].avg_ms, Some(8.0));
        assert_eq!(report[0].response_bytes, Some(1234.0));
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

        // `digest_id`の無い過去のrunで切った文が衝突していても、回数と時間は落とさない。
        let collided = database_queries(&[
            metric("db.query.calls", 10.0, &db_labels),
            metric("db.query.total_duration", 1_000.0, &db_labels),
            metric("db.query.p95_duration", 100.0, &db_labels),
            metric("db.query.calls", 20.0, &db_labels),
            metric("db.query.total_duration", 8_000.0, &db_labels),
            metric("db.query.p95_duration", 400.0, &db_labels),
        ]);
        assert_eq!(collided.len(), 1);
        assert_eq!(collided[0].calls, 30.0);
        assert_eq!(collided[0].total_ms, 9_000.0);
        assert_eq!(collided[0].p95_ms, None);

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
        assert_eq!(host[0].value, Some(50.0));
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
    fn cores_and_quantiles_are_not_merged_into_one_row() {
        let metric = |name: &str, value: f64, labels: &[(&str, &str)]| Metric {
            name: name.into(),
            value,
            unit: "percent".into(),
            timestamp: None,
            labels: labels
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        };
        // coreやquantileを1行に畳むと、存在しない値（core平均、分位点の平均）が出る。
        let cores = host_metrics(
            &[],
            &[
                metric(
                    "host.core_busy_percent",
                    100.0,
                    &[("node", "app1"), ("core", "0")],
                ),
                metric(
                    "host.core_busy_percent",
                    0.0,
                    &[("node", "app1"), ("core", "1")],
                ),
            ],
        );
        assert_eq!(cores.len(), 2);
        assert_eq!(cores[0].labels.get("core").map(String::as_str), Some("0"));
        assert_eq!(cores[0].value, Some(100.0));
        assert_eq!(cores[1].value, Some(0.0));

        let gaps = client_metrics(
            &[],
            &[
                metric(
                    "client.request_gap",
                    0.001,
                    &[("node", "app1"), ("quantile", "0.50")],
                ),
                metric(
                    "client.request_gap",
                    0.010,
                    &[("node", "app1"), ("quantile", "0.95")],
                ),
                metric(
                    "client.request_gap",
                    0.100,
                    &[("node", "app1"), ("quantile", "0.99")],
                ),
            ],
        );
        assert_eq!(gaps.len(), 3);
        for row in &gaps {
            assert_eq!(
                row.aggregation,
                crate::metric_semantics::MetricAggregation::MaxOfQuantile
            );
        }
        assert_eq!(
            gaps.iter()
                .find(|row| row.labels["quantile"] == "0.99")
                .unwrap()
                .value,
            Some(0.100)
        );
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

    #[test]
    fn coverage_is_scoped_to_node_and_collector_without_hiding_failures() {
        let collector = |name: &str, node: &str, status: &str| CollectorResult {
            name: name.into(),
            node: Some(node.into()),
            phase: "after".into(),
            status: status.into(),
            exit_code: (status == "complete").then_some(0),
            error: None,
            log_ids: Vec::new(),
        };
        let metric = |name: &str, node: &str, collector: &str| Metric {
            name: name.into(),
            value: 1.0,
            unit: String::new(),
            timestamp: None,
            labels: BTreeMap::from([
                ("node".into(), node.into()),
                ("collector".into(), collector.into()),
            ]),
        };
        let collectors = vec![
            collector("alp", "app1", "complete"),
            collector("alp", "app2", "failed"),
            collector("host-sampler", "app1", "complete"),
            collector("sysstat", "app2", "unavailable"),
        ];
        let partial = coverage(
            &[
                collector("slp", "db1", "complete"),
                collector("slp", "db2", "complete"),
            ],
            &[
                metric("db.query.calls", "db1", "slp"),
                metric("db.query.total_duration", "db1", "slp"),
                metric("db.slow_log_unclassified", "db1", "slp"),
                metric("db.query.calls", "db2", "slp"),
                metric("db.query.total_duration", "db2", "slp"),
            ],
            &[],
        );
        let db1 = partial.iter().find(|row| row.node == "db1").unwrap();
        assert_eq!(db1.status, "partial");
        assert!(db1.error.as_ref().unwrap().contains("1 slow-log records"));
        assert_eq!(
            partial.iter().find(|row| row.node == "db2").unwrap().status,
            "complete"
        );
        let metrics = vec![
            metric("http.requests", "app1", "alp"),
            metric("http.request_duration", "app1", "alp"),
            metric("host.cpu_percent", "app1", "host-sampler"),
        ];
        let report = coverage(&collectors, &metrics, &[]);
        assert!(report.iter().any(|item| {
            item.section == "http"
                && item.node == "app1"
                && item.collector == "alp"
                && item.status == "complete"
        }));
        assert!(report.iter().any(|item| {
            item.section == "http"
                && item.node == "app2"
                && item.collector == "alp"
                && item.status == "failed"
        }));
        assert!(report.iter().any(|item| {
            item.section == "host"
                && item.node == "app1"
                && item.collector == "host-sampler"
                && item.status == "missing"
                && item.missing_metrics == ["host.memory_used_bytes"]
        }));
        assert!(report.iter().any(|item| {
            item.section == "host"
                && item.node == "app2"
                && item.collector == "sysstat"
                && item.status == "unavailable"
        }));
        assert!(report.iter().any(|item| {
            item.section == "profiles"
                && item.node == "-"
                && item.collector == "-"
                && item.status == "missing"
        }));

        // 分類の一覧に無いcollectorも、失敗はerrorと一緒に載る（成功や不在は載せない）。
        let mut status = collector("mysql-status", "db1", "failed");
        status.error = Some("mysql: access denied".into());
        let report = coverage(
            &[status, collector("service-throttle", "app1", "unavailable")],
            &[],
            &[],
        );
        let other = report
            .iter()
            .filter(|item| item.section == "other")
            .collect::<Vec<_>>();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].collector, "mysql-status");
        assert_eq!(other[0].status, "failed");
        assert_eq!(other[0].error.as_deref(), Some("mysql: access denied"));
    }
}
