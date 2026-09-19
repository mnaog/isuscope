use crate::{
    model::{BenchmarkMessageKind, RunManifest, Transition},
    query::{self, MetricQueryOutput, MetricQueryRow},
    report::{
        CoverageSummary, CpuSummary, DatabaseSummary, HostSummary, HttpRouteSummary,
        ProfileArtifact, RunDiagnostics, UpstreamSummary,
    },
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Serialize)]
pub struct BriefOutput {
    pub schema_version: u32,
    pub review: Option<crate::changes::RunReview>,
    pub run: BriefRun,
    pub coverage_issues: BriefSection<CoverageIssueGroup>,
    pub coverage_info_count: usize,
    pub benchmark: BriefSection<MetricQueryRow>,
    /// Values read from the system under test to work out what the score is made of.
    /// Collected in `survey-run`, where that question is settled.
    pub score_inputs: BriefSection<MetricQueryRow>,
    /// Parser-kept benchmark output lines: why it failed and what the errors were.
    pub benchmark_messages: BriefBenchmarkMessages,
    pub http: BriefSection<HttpRouteSummary>,
    /// DBの行を要約した区間（`load`など）。区間を持たない古いrunでは出さない。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_window: Option<String>,
    pub database: BriefSection<DatabaseSummary>,
    pub omitted_alternative_database_rows: usize,
    pub cpu: BriefSection<CpuSummary>,
    /// `hosts`と`clients`を要約した区間。initializeの終わりが分かるrunは`load`、
    /// 分からないrunは`whole`（initializeを含む）。
    pub hosts_window: &'static str,
    /// 1 node 1行。詳細は`query --scope series --window load`で掘ります。
    pub hosts: Vec<BriefHostNode>,
    /// ベンチ側の接続の使い方。access logに`$connection`と`$msec`があるとき、1 node 1行。
    pub clients: Vec<BriefClientNode>,
    /// Per backend, when the access log carries `$upstream_addr` and the upstream times.
    pub upstreams: BriefSection<UpstreamSummary>,
    pub transitions: BriefSection<Transition>,
    pub artifact_issues: BriefSection<ProfileArtifact>,
    pub unavailable_artifact_count: usize,
    pub warnings: Vec<String>,
}

/// nodeごとの1行。全nodeの状況を最初の画面で見切れるようにするための要約で、
/// 個々のmetricはここから`query`へ進みます。
#[derive(Debug, Serialize)]
pub struct BriefHostNode {
    pub node: String,
    pub cpu_busy_percent: Option<f64>,
    pub cpu_busy_peak_percent: Option<f64>,
    /// 最も詰まっていたコアのピーク。全体に余裕があっても1コアだけ飽和する構成を見落とさない。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busiest_core_peak_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iowait_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steal_percent: Option<f64>,
    /// PSIのうち最も高かったもの（resource名とピーク）。taskが資源待ちで止まった時間の割合。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure: Option<BriefPressure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load1_peak: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_used_peak_bytes: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_util_peak_percent: Option<f64>,
    /// CPUを多く使っていたserviceの上位。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub top_services: Vec<BriefService>,
    /// このnodeの詳細行数。`query`で何件に当たるかの目安。
    pub detail_rows: usize,
}

/// ベンチ側がそのnodeへの接続をどう使ったか。
#[derive(Debug, Serialize)]
pub struct BriefClientNode {
    pub node: String,
    /// 最初の要求から最後の応答までを積んだ同時接続数（遊休中の保持は含まない）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections_in_use: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections_in_use_peak: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections_opened_per_second: Option<f64>,
    /// 応答を返してから同じ接続に次の要求が来るまで（ms）。5秒ごとの分位のうち最も大きい値。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub request_gap_ms: BTreeMap<String, f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_per_connection: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct BriefPressure {
    pub resource: String,
    pub peak_percent: f64,
}

#[derive(Debug, Serialize)]
pub struct BriefService {
    pub service: String,
    pub cpu_cores_peak: f64,
}

#[derive(Debug, Serialize)]
pub struct BriefRun {
    pub id: String,
    /// The form printed by `run` and accepted everywhere a run is named.
    pub short_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub state: String,
    pub score: Option<i64>,
    pub passed: Option<bool>,
    pub hypothesis: String,
    pub analysis_status: String,
    pub commit_hash: Option<String>,
    pub dirty: bool,
    /// Organizer-only benchmark lines dropped before saving, per `operator_line_pattern`.
    pub operator_lines_dropped: usize,
    pub metric_count: usize,
}

#[derive(Debug, Serialize, Default)]
pub struct BriefBenchmarkMessages {
    pub failure: Vec<String>,
    pub errors: Vec<BriefErrorSamples>,
    pub omitted_count: usize,
}

#[derive(Debug, Serialize)]
pub struct BriefErrorSamples {
    pub category: Option<String>,
    pub samples: Vec<String>,
}

pub fn benchmark_messages(run: &RunManifest) -> BriefBenchmarkMessages {
    let mut errors = Vec::<BriefErrorSamples>::new();
    for message in run.benchmark_messages(BenchmarkMessageKind::Error) {
        match errors
            .iter_mut()
            .find(|group| group.category == message.category)
        {
            Some(group) => group.samples.push(message.text.clone()),
            None => errors.push(BriefErrorSamples {
                category: message.category.clone(),
                samples: vec![message.text.clone()],
            }),
        }
    }
    BriefBenchmarkMessages {
        failure: run.failure_reasons(),
        errors,
        omitted_count: run
            .enrichments
            .iter()
            .map(|enrichment| enrichment.omitted_message_count)
            .sum(),
    }
}

#[derive(Debug, Serialize)]
pub struct BriefSection<T> {
    pub total_count: usize,
    pub truncated: bool,
    pub items: Vec<T>,
}

#[derive(Debug, Serialize)]
pub struct CoverageIssueGroup {
    pub severity: &'static str,
    pub section: String,
    pub collector: String,
    pub phase: String,
    pub status: String,
    pub nodes: Vec<String>,
    pub missing_metrics: Vec<String>,
    /// 失敗したcollectorのerror（同じものは1つにまとめる）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    pub occurrences: usize,
}

pub fn build(
    diagnostics: RunDiagnostics,
    benchmark: MetricQueryOutput,
    score_inputs: MetricQueryOutput,
    limit: usize,
) -> BriefOutput {
    let run = diagnostics.run;
    let (coverage_issues, coverage_info_count) = coverage_issues(diagnostics.coverage);
    let mut http = diagnostics.http;
    http.iter_mut().for_each(query::round_http_summary);
    let (mut database, omitted_alternative_database_rows, database_window) =
        preferred_database(diagnostics.database, &run);
    database.iter_mut().for_each(query::round_database_summary);
    let hosts_window = diagnostics.host_window;
    // clientの値を要約した区間の長さ。loadならinitializeの終わりから、wholeならベンチの始まりから。
    let window_seconds = match hosts_window {
        "load" => run.benchmark.initialize_finished_at,
        _ => run.benchmark.started_at,
    }
    .zip(run.benchmark.finished_at)
    .and_then(|(start, end)| (end - start).num_microseconds())
    .map(|micros| micros as f64 / 1_000_000.0)
    .filter(|seconds| *seconds > 0.0);
    let clients = client_nodes(&diagnostics.client, window_seconds);
    let hosts = host_nodes(&diagnostics.host);
    let benchmark_messages = benchmark_messages(&run);
    let unavailable_artifact_count = diagnostics
        .artifacts
        .iter()
        .filter(|item| item.status == "unavailable")
        .count();
    BriefOutput {
        schema_version: 1,
        review: None,
        run: BriefRun {
            short_id: crate::runner::short_id(&run.id).into(),
            id: run.id,
            started_at: run.started_at.to_rfc3339(),
            finished_at: run.finished_at.map(|value| value.to_rfc3339()),
            state: run.state.as_str().into(),
            score: run.benchmark.score,
            passed: run.benchmark.passed,
            hypothesis: run.hypothesis,
            analysis_status: run.analysis_status.as_str().into(),
            operator_lines_dropped: run.benchmark.operator_lines_dropped,
            commit_hash: run.source.commit_hash,
            dirty: run.source.dirty,
            metric_count: run.metric_count,
        },
        coverage_issues: section(coverage_issues, limit),
        coverage_info_count,
        benchmark: section(by_metric_then_magnitude(benchmark.rows), limit),
        score_inputs: section(score_inputs.rows, limit),
        benchmark_messages,
        http: section(http, limit),
        database_window,
        database: section(database, limit),
        omitted_alternative_database_rows,
        cpu: section(diagnostics.cpu, limit),
        hosts_window,
        hosts,
        clients,
        upstreams: section(diagnostics.upstreams, limit),
        transitions: section(diagnostics.transitions, limit),
        artifact_issues: section(
            diagnostics
                .artifacts
                .into_iter()
                .filter(|item| item.status == "failed")
                .collect(),
            limit,
        ),
        unavailable_artifact_count,
        warnings: benchmark.warnings,
    }
}

fn coverage_issues(coverage: Vec<CoverageSummary>) -> (Vec<CoverageIssueGroup>, usize) {
    type Key = (String, String, String, String, Vec<String>);
    let mut groups = BTreeMap::<Key, (Vec<String>, BTreeSet<String>)>::new();
    let mut info_count = 0;
    for item in coverage {
        let severity = match item.status.as_str() {
            "failed" => "critical",
            "missing" => "warning",
            _ if item.missing_metrics.is_empty() => continue,
            _ => "info",
        };
        if severity == "info" {
            info_count += 1;
            continue;
        }
        let (nodes, errors) = groups
            .entry((
                item.section,
                item.collector,
                item.phase,
                item.status,
                item.missing_metrics,
            ))
            .or_default();
        nodes.push(item.node);
        errors.extend(item.error);
    }
    let mut issues = groups
        .into_iter()
        .map(
            |((section, collector, phase, status, missing_metrics), (mut nodes, errors))| {
                nodes.sort();
                nodes.dedup();
                CoverageIssueGroup {
                    severity: if status == "failed" {
                        "critical"
                    } else {
                        "warning"
                    },
                    section,
                    collector,
                    phase,
                    status,
                    occurrences: nodes.len(),
                    nodes,
                    missing_metrics,
                    errors: errors.into_iter().collect(),
                }
            },
        )
        .collect::<Vec<_>>();
    issues.sort_by(|a, b| {
        severity_rank(a.severity)
            .cmp(&severity_rank(b.severity))
            .then_with(|| a.section.cmp(&b.section))
            .then_with(|| a.collector.cmp(&b.collector))
    });
    (issues, info_count)
}

fn severity_rank(value: &str) -> u8 {
    match value {
        "critical" => 0,
        "warning" => 1,
        _ => 2,
    }
}

/// DBの行を1つのsource・1つの区間へ絞る。区間ごとに集計されていれば負荷区間だけを使い、
/// initializeの一括INSERTなどが上位を占めないようにする。
/// DB欄に出す区間を、行の有無ではなくrunの記録から決める。負荷区間のSQLを無くせた（loadが
/// 0件）runで、行のあるinitializeへ戻って表示しないように、loadは0件なら空のまま出す。
/// 取れなかったこと（slpの失敗）はcoverageに出る。
fn preferred_database(
    database: Vec<DatabaseSummary>,
    run: &crate::model::RunManifest,
) -> (Vec<DatabaseSummary>, usize, Option<String>) {
    let total_count = database.len();
    // slp collectorが区間に分けて集計したrun。行が1つも無くても、slpが成功していれば区間は決まる。
    let windowed = database.iter().any(|item| item.window.is_some())
        || (database.is_empty()
            && run
                .collectors
                .iter()
                .any(|collector| collector.name == "slp" && collector.status == "complete"));
    // slpは負荷の始まりが分かればinitializeとloadに、分からなければwholeに分ける。
    let split = run.benchmark.initialize_finished_at.is_some()
        || database
            .iter()
            .any(|item| matches!(item.window.as_deref(), Some("load" | "initialize")));
    let window = windowed.then(|| if split { "load" } else { "whole" }.to_owned());
    let database = match &window {
        Some(window) => database
            .into_iter()
            .filter(|item| item.window.as_deref() == Some(window.as_str()))
            .collect::<Vec<_>>(),
        // 区間を持たない古いrun。slow logを手元で解析した行があればslpの行より優先する。
        None if database.iter().any(|item| item.source == "mysql-log-delta") => database
            .into_iter()
            .filter(|item| item.source != "slp")
            .collect(),
        None => database,
    };
    let omitted = total_count - database.len();
    (database, omitted, window)
}

/// 同じmetricの中では値の大きい順に並べる。名前順のまま切ると、件数の多いエラーより
/// 名前が先のものが残ってしまう。
fn by_metric_then_magnitude(mut rows: Vec<MetricQueryRow>) -> Vec<MetricQueryRow> {
    rows.sort_by(|a, b| {
        a.metric.cmp(&b.metric).then_with(|| {
            b.value
                .unwrap_or(f64::NEG_INFINITY)
                .total_cmp(&a.value.unwrap_or(f64::NEG_INFINITY))
        })
    });
    rows
}

/// clientの要約をnodeごとに1行へ畳む。名前順に5件だけ出すと、`request_gap`が必ず落ちる。
/// `window_seconds`は要約した区間の長さ。新規接続数は区間の合計なので、これで割って毎秒にする。
fn client_nodes(rows: &[HostSummary], window_seconds: Option<f64>) -> Vec<BriefClientNode> {
    let mut nodes: BTreeMap<&str, Vec<&HostSummary>> = BTreeMap::new();
    for row in rows {
        nodes.entry(row.node.as_str()).or_default().push(row);
    }
    nodes
        .into_iter()
        .map(|(node, rows)| {
            let find = |name: &str| rows.iter().find(|row| row.metric == name);
            let request_gap_ms = rows
                .iter()
                .filter(|row| row.metric == "client.request_gap")
                .filter_map(|row| {
                    let quantile = row.labels.get("quantile")?;
                    Some((
                        format!("p{}", quantile.trim_start_matches("0.")),
                        query::round_to(row.peak, 3),
                    ))
                })
                .collect();
            BriefClientNode {
                node: node.into(),
                connections_in_use: find("client.connections_in_use")
                    .and_then(|row| row.value)
                    .map(|value| query::round_to(value, 1)),
                connections_in_use_peak: find("client.connections_in_use")
                    .map(|row| query::round_to(row.peak, 1)),
                // 新規接続数は加算できる値で、`value`は区間の合計（1 bucketぶんではない）。
                connections_opened_per_second: find("client.connections_opened")
                    .and_then(|row| row.value)
                    .zip(window_seconds)
                    .map(|(total, seconds)| query::round_to(total / seconds, 1)),
                request_gap_ms,
                requests_per_connection: find("client.connection_requests_mean")
                    .and_then(|row| row.value)
                    .map(|value| query::round_to(value, 2)),
            }
        })
        .collect()
}

/// nodeごとに1行へ畳む。全体像を見る入口なので、件数ではなく網羅を優先する。
fn host_nodes(rows: &[HostSummary]) -> Vec<BriefHostNode> {
    let mut nodes: BTreeMap<&str, Vec<&HostSummary>> = BTreeMap::new();
    for row in rows {
        nodes.entry(row.node.as_str()).or_default().push(row);
    }
    nodes
        .into_iter()
        .map(|(node, rows)| {
            let average = |name: &str| {
                rows.iter()
                    .filter(|row| row.metric == name)
                    .filter_map(|row| row.value)
                    .reduce(f64::max)
                    .map(|value| query::round_to(value, 2))
            };
            let peak = |name: &str| {
                rows.iter()
                    .filter(|row| row.metric == name)
                    .map(|row| row.peak)
                    .reduce(f64::max)
                    .map(|value| query::round_to(value, 2))
            };
            let busiest_core_peak_percent =
                peak("host.core_busy_max_percent").or_else(|| peak("host.core_busy_percent"));
            let pressure = rows
                .iter()
                .filter(|row| {
                    row.metric.starts_with("host.psi_") && row.metric.ends_with("_some_percent")
                })
                .max_by(|a, b| a.peak.total_cmp(&b.peak))
                .map(|row| BriefPressure {
                    resource: row
                        .metric
                        .trim_start_matches("host.psi_")
                        .trim_end_matches("_some_percent")
                        .into(),
                    peak_percent: query::round_to(row.peak, 2),
                });
            let mut services = rows
                .iter()
                .filter(|row| row.metric == "service.cpu_cores")
                .map(|row| BriefService {
                    service: row.target.clone(),
                    cpu_cores_peak: query::round_to(row.peak, 3),
                })
                .collect::<Vec<_>>();
            services.sort_by(|a, b| b.cpu_cores_peak.total_cmp(&a.cpu_cores_peak));
            services.truncate(3);
            BriefHostNode {
                node: node.into(),
                cpu_busy_percent: average("host.cpu_busy_percent")
                    .or_else(|| average("host.cpu_percent")),
                cpu_busy_peak_percent: peak("host.cpu_busy_percent")
                    .or_else(|| peak("host.cpu_percent")),
                busiest_core_peak_percent,
                iowait_percent: average("host.cpu_iowait_percent"),
                steal_percent: average("host.cpu_steal_percent"),
                pressure,
                load1_peak: peak("host.load1"),
                memory_used_peak_bytes: peak("host.memory_used_bytes"),
                disk_util_peak_percent: peak("host.disk_util_percent"),
                top_services: services,
                detail_rows: rows.len(),
            }
        })
        .collect()
}

fn section<T>(mut items: Vec<T>, limit: usize) -> BriefSection<T> {
    let total_count = items.len();
    items.truncate(limit);
    BriefSection {
        total_count,
        truncated: total_count > items.len(),
        items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_shows_an_empty_load_rather_than_falling_back_to_initialize() {
        // 負荷区間のSQLを無くせたrun（initializeだけに行がある）で、initializeの行を出さない。
        let row = |window: &str| DatabaseSummary {
            node: "db1".into(),
            engine: "mysql".into(),
            digest: "INSERT INTO t VALUES (?)".into(),
            source: "slp".into(),
            window: Some(window.into()),
            calls: 100.0,
            ..Default::default()
        };
        let run: crate::model::RunManifest = serde_json::from_value(serde_json::json!({
            "schema_version": 6, "id": "r", "mode": "run", "state": "complete",
            "started_at": "2026-09-19T00:00:00Z", "finished_at": null,
            "source": {"repository": ".", "git_available": false, "commit_hash": null,
                "branch": null, "dirty": false, "state_sha256": "", "untracked": [], "error": null},
            "benchmark": {"mode": "command", "command": [], "exit_code": 0, "score": 1,
                "passed": true, "messages": [], "initialize_started_at": "2026-09-19T00:00:01Z",
                "initialize_finished_at": "2026-09-19T00:00:10Z", "error": null},
            "collectors": [{"name": "slp", "node": "db1", "phase": "after", "status": "complete",
                "exit_code": 0, "error": null, "log_ids": []}],
            "logs": [], "metric_count": 0, "transition_count": 0
        }))
        .unwrap();
        let (rows, omitted, window) = preferred_database(vec![row("initialize")], &run);
        assert_eq!(window.as_deref(), Some("load"));
        assert!(rows.is_empty());
        assert_eq!(omitted, 1);
        // slpが成功して1行も無いrunも、負荷区間は0件として出す。
        let (rows, _, window) = preferred_database(Vec::new(), &run);
        assert_eq!((rows.len(), window.as_deref()), (0, Some("load")));
    }

    #[test]
    fn new_connections_per_second_divide_the_window_total_by_its_length() {
        // 60秒の負荷で、5秒bucketごとに10接続（合計120）なら毎秒2。1 bucketの5秒で割ると24になる。
        let row = HostSummary {
            node: "app1".into(),
            metric: "client.connections_opened".into(),
            target: "host".into(),
            source: "alp".into(),
            labels: BTreeMap::new(),
            unit: "connections".into(),
            aggregation: crate::metric_semantics::MetricAggregation::Sum,
            value: Some(120.0),
            peak: 10.0,
            peak_at: None,
            samples: 12,
        };
        let nodes = client_nodes(std::slice::from_ref(&row), Some(60.0));
        assert_eq!(nodes[0].connections_opened_per_second, Some(2.0));
        // 区間の長さが分からなければ、毎秒には直さない。
        assert_eq!(
            client_nodes(&[row], None)[0].connections_opened_per_second,
            None
        );
    }

    #[test]
    fn coverage_issues_group_nodes_and_hide_info_rows() {
        let coverage = ["app2", "app3"]
            .into_iter()
            .map(|node| CoverageSummary {
                section: "http".into(),
                node: node.into(),
                collector: "alp".into(),
                phase: "after".into(),
                status: "missing".into(),
                missing_metrics: vec!["http.requests".into()],
                error: None,
            })
            .chain(std::iter::once(CoverageSummary {
                section: "cpu".into(),
                node: "app1".into(),
                collector: "perf".into(),
                phase: "after".into(),
                status: "unavailable".into(),
                missing_metrics: vec!["cpu.sample_count".into()],
                error: None,
            }))
            .collect();
        let (issues, info_count) = coverage_issues(coverage);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].nodes, ["app2", "app3"]);
        assert_eq!(issues[0].occurrences, 2);
        assert_eq!(info_count, 1);
    }
}
