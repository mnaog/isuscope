use crate::{
    model::{BenchmarkMessageKind, Metric, RunManifest, Transition},
    query::{self, MetricQueryOutput, MetricQueryRow},
    report::{
        CoverageSummary, CpuSummary, DatabaseSummary, HostSummary, HttpRouteSummary,
        ProfileArtifact, RunDiagnostics, UpstreamSummary,
    },
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// briefの`review.changes`に出す変更の数。
const REVIEW_CHANGES: usize = 20;

/// 長文の冒頭だけを出す文字数。全文は各欄の`full_text`の命令で読む。
const EXCERPT_CHARS: usize = 240;

#[derive(Debug, Serialize)]
pub struct BriefOutput {
    pub schema_version: u32,
    /// このrunの短縮ID。ほかの出力の`run`と同じ。
    pub run: String,
    pub summary: BriefRun,
    pub review: Option<BriefReview>,
    pub coverage_issues: BriefSection<CoverageIssueGroup>,
    pub coverage_notes_hidden: usize,
    pub benchmark: BriefSection<MetricQueryRow>,
    /// Values read from the system under test to work out what the score is made of.
    /// Collected in `survey-run`, where that question is settled.
    pub score_inputs: BriefSection<MetricQueryRow>,
    /// Parser-kept benchmark output lines: why it failed and what the errors were.
    pub benchmark_messages: BriefBenchmarkMessages,
    /// ベンチの間にアプリ（`service_units`）・nginxのerror log・kernelが出したエラーの型。多い順。
    pub logs: BriefSection<BriefLogPattern>,
    pub http: BriefSection<HttpRouteSummary>,
    /// `window`はDBの行を要約した区間（`load`など）。区間を持たない古いrunでは付かない。
    pub database: BriefSection<DatabaseSummary>,
    pub database_rows_filtered_out: usize,
    /// MySQLがfileの読み書きで待った時間の長い順。ベンチの前後の差なのでinitializeも含む。
    pub database_io: BriefSection<BriefDatabaseFile>,
    /// buffer poolの大きさと、tableのdataとindexの合計（ベンチ後）。
    pub database_memory: BriefSection<BriefDatabaseMemory>,
    /// idle taskが待機していた時間（`swapper`の`native_safe_halt`など）は除いて順位を付けます。
    /// `sample_percent`は除く前の全sampleに対する割合のままです。
    pub cpu: BriefSection<CpuSummary>,
    /// 1 node 1行。詳細は`query --scope series --window load`で掘ります。
    /// 遊んでいたnodeは`quiet_hosts`へまとめ、ここには出しません。`window`は要約した区間で、
    /// initializeの終わりが分かるrunは`load`、分からないrunは`whole`（initializeを含む）。
    pub hosts: BriefSection<BriefHostNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiet_hosts: Option<BriefQuietHosts>,
    /// ベンチ側の接続の使い方。access logに`$connection`と`$msec`があるとき、1 node 1行。
    /// `window`は`hosts`と同じ区間。
    pub clients: BriefSection<BriefClientNode>,
    /// Per backend, when the access log carries `$upstream_addr` and the upstream times.
    pub upstreams: BriefSection<UpstreamSummary>,
    pub transitions: BriefSection<BriefTransition>,
    pub artifact_issues: BriefSection<ProfileArtifact>,
    pub profiles_unavailable: usize,
    /// 作業の流れで次に使えるコマンド。比較元があるときのHTTPとDBの比較だけで、推測の助言は入れない。
    /// briefは並行して作業する別の人も読むので、書き込む`analyze`は出さない（runを走らせた本人へは
    /// `run`の終了時に出る）。無ければ空。
    pub next: Vec<String>,
    pub warnings: Vec<String>,
}

impl BriefOutput {
    /// 順位を見る表を、どれも`rows`行までにする。briefが出力の上限を超えるときに使う。
    /// node単位の表（`hosts`・`clients`・`database_memory`）は全nodeを見切るためのものなので切らない。
    pub fn cut_tables(&mut self, rows: usize) {
        self.coverage_issues.cut(rows);
        self.benchmark.cut(rows);
        self.score_inputs.cut(rows);
        self.logs.cut(rows);
        self.http.cut(rows);
        self.database.cut(rows);
        self.database_io.cut(rows);
        self.cpu.cut(rows);
        self.upstreams.cut(rows);
        self.transitions.cut(rows);
        self.artifact_issues.cut(rows);
    }
}

/// nodeごとの1行。全nodeの状況を最初の画面で見切れるようにするための要約で、
/// 個々のmetricはここから`query`へ進みます。
#[derive(Debug, Serialize)]
pub struct BriefHostNode {
    pub node: String,
    pub cpu_busy_avg_percent: Option<f64>,
    pub cpu_busy_max_percent: Option<f64>,
    /// 最も詰まっていたコアのピーク。全体に余裕があっても1コアだけ飽和する構成を見落とさない。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busiest_core_max_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iowait_avg_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steal_avg_percent: Option<f64>,
    /// PSIのうち最も高かったもの（resource名とピーク）。taskが資源待ちで止まった時間の割合。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure: Option<BriefPressure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load1_max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_used_max_mib: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_util_max_percent: Option<f64>,
    /// CPUを多く使っていたserviceの上位。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub top_services: Vec<BriefService>,
}

/// どの値も閾値を下回っていたnode。負荷を振り分ける余地として、名前とその中の最大値だけ残す。
#[derive(Debug, Serialize)]
pub struct BriefQuietHosts {
    pub nodes: Vec<String>,
    pub cpu_busy_max_percent: f64,
    pub busiest_core_max_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iowait_avg_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure_max_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_util_max_percent: Option<f64>,
}

/// 遷移1件と、その順序がどこまで確かか。順序の値は遷移helperが記録したrunだけに付く。
#[derive(Debug, Serialize)]
pub struct BriefTransition {
    pub from_route: String,
    pub to_route: String,
    pub count: i64,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    /// 前の要求が終わる前に次の要求が始まった回数（並行して出た要求で、遷移とは限らない）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlap_count: Option<f64>,
    /// 開始時刻が同じか、開始時刻を推定できない旧形式のlogで、順序が決まらなかった回数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ambiguous_count: Option<f64>,
}

fn transitions(
    transitions: Vec<Transition>,
    order: &crate::report::TransitionOrder,
) -> Vec<BriefTransition> {
    transitions
        .into_iter()
        .map(|transition| {
            let edge = order
                .edges
                .get(&(transition.from_route.clone(), transition.to_route.clone()))
                .copied()
                .unwrap_or_default();
            BriefTransition {
                from_route: transition.from_route,
                to_route: transition.to_route,
                count: transition.count,
                p50_ms: transition.p50_ms,
                p95_ms: transition.p95_ms,
                overlap_count: edge.overlap_count,
                ambiguous_count: edge.ambiguous_count,
            }
        })
        .collect()
}

/// [`crate::changes::RunReview`]から判断に要る部分だけを残したもの。長文は冒頭だけにし、
/// run・commitは短縮形にする。
#[derive(Debug, Serialize)]
pub struct BriefReview {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_analysis: Option<BriefAnalysis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<BriefComparison>,
    pub changes: BriefSection<BriefChange>,
}

#[derive(Debug, Serialize)]
pub struct BriefAnalysis {
    pub verdict: crate::model::AnalysisVerdict,
    /// 比較元runの短縮ID。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_text: Option<String>,
}

#[derive(Debug, Serialize)]
/// 比較は`query --base`と同じ書き方。比較元は`_base`、今回は接尾辞なし、差は`_delta`と`_delta_percent`。
pub struct BriefComparison {
    /// 比較元runの短縮ID。
    pub base: String,
    pub score_base: Option<i64>,
    pub score: Option<i64>,
    pub score_delta: Option<i64>,
    pub score_delta_percent: Option<f64>,
    pub conditions: Vec<crate::changes::ComparisonCondition>,
}

#[derive(Debug, Serialize)]
pub struct BriefChange {
    pub id: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// 最新の判断。まだ判断していない変更では出さない。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    /// 理由が`latest_analysis.body`と同じ文章なら出さず、`reason_same_as_analysis`を立てる。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reason_same_as_analysis: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revisit: Option<String>,
    /// 判断の根拠にしたrun。`短縮ID commit先頭12桁`、未commitの変更があれば` dirty`を付ける。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_text: Option<String>,
}

pub fn review(review: crate::changes::RunReview) -> BriefReview {
    let short = |id: &str| crate::runner::short_id(id).to_owned();
    let analysis_body = review
        .latest_analysis
        .as_ref()
        .map(|analysis| analysis.body.clone());
    let latest_analysis = review.latest_analysis.map(|analysis| {
        let (body, cut) = excerpt(&analysis.body);
        BriefAnalysis {
            verdict: analysis.verdict,
            base: analysis.base_run_id.as_deref().map(short),
            body,
            full_text: cut.then(|| {
                format!(
                    "isuscope sql \"SELECT body FROM run_analyses WHERE id='{}'\"",
                    analysis.id
                )
            }),
        }
    });
    let comparison = review.comparison.map(|comparison| BriefComparison {
        base: short(&comparison.base_run_id),
        score_base: comparison.score.base,
        score: comparison.score.candidate,
        score_delta: comparison.score.delta,
        score_delta_percent: comparison.score.delta_percent,
        conditions: comparison.conditions,
    });
    let changes = review
        .changes
        .into_iter()
        .map(|summary| {
            let change = summary.change;
            let (description, mut cut) = excerpt(&change.description);
            let decision = summary.latest_decision;
            let same = decision
                .as_ref()
                .is_some_and(|decision| Some(&decision.reason) == analysis_body.as_ref());
            let reason = decision.as_ref().filter(|_| !same).map(|decision| {
                let (reason, reason_cut) = excerpt(&decision.reason);
                cut |= reason_cut;
                reason
            });
            BriefChange {
                full_text: cut.then(|| format!("isuscope change show {}", change.id)),
                id: change.id,
                description,
                target: change.target,
                status: decision.as_ref().map(|decision| decision.status.as_str()),
                decided_at: decision
                    .as_ref()
                    .map(|decision| crate::model::display_time(decision.created_at)),
                reason,
                reason_same_as_analysis: same,
                revisit: decision
                    .as_ref()
                    .and_then(|decision| decision.revisit.clone()),
                evidence: decision
                    .map(|decision| {
                        decision
                            .evidence
                            .iter()
                            .map(crate::changes::evidence_label)
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
        .collect();
    BriefReview {
        latest_analysis,
        comparison,
        changes: section(changes, REVIEW_CHANGES),
    }
}

fn excerpt(text: &str) -> (String, bool) {
    crate::model::excerpt(text, EXCERPT_CHARS)
}

/// ベンチ側がそのnodeへの接続をどう使ったか。
#[derive(Debug, Serialize)]
pub struct BriefClientNode {
    pub node: String,
    /// 最初の要求から最後の応答までを積んだ同時接続数（遊休中の保持は含まない）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections_in_use_avg: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections_in_use_max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections_opened_per_second: Option<f64>,
    /// 応答を返してから同じ接続に次の要求が来るまで（ms）。5秒ごとの分位のうち最も大きい値。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub request_gap_ms: BTreeMap<String, f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_per_connection_avg: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct BriefPressure {
    pub resource: String,
    pub max_percent: f64,
}

#[derive(Debug, Serialize)]
pub struct BriefService {
    pub service: String,
    pub cpu_max_cores: f64,
}

#[derive(Debug, Serialize)]
pub struct BriefRun {
    /// 出さない。runはトップレベルの`run`（短縮ID）で指す。
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    pub short_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub state: String,
    pub score: Option<i64>,
    pub passed: Option<bool>,
    pub hypothesis: String,
    pub analysis_status: String,
    /// 先頭12桁。ほかの出力の短縮commitと同じ。
    #[serde(serialize_with = "crate::model::serialize_short_commit")]
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
    /// 行を要約した区間（`load`など）。区間で分けて要約した欄だけに付く。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// 切ったときだけ付く、残りの行を見るコマンド。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub more: Option<String>,
    pub items: Vec<T>,
    /// 残りの行を見るコマンド。後で[`BriefOutput::cut_tables`]が切ったときに`more`へ出す。
    #[serde(skip)]
    more_command: Option<String>,
}

impl<T> BriefSection<T> {
    fn window(mut self, window: Option<String>) -> Self {
        self.window = window;
        self
    }

    fn more(mut self, command: impl FnOnce() -> String) -> Self {
        let command = command();
        if self.truncated {
            self.more = Some(command.clone());
        }
        self.more_command = Some(command);
        self
    }

    fn cut(&mut self, rows: usize) {
        if self.items.len() > rows {
            self.items.truncate(rows);
            self.truncated = true;
            self.more = self.more_command.clone();
        }
    }
}

/// logの1つの型（数字を含む語を`<N>`へ置き換えた行）。briefには型の最初の1行だけを出す。
/// 型そのものはほぼ同じ文の繰り返しになるので、`query --metric log.error_lines`で見る。
#[derive(Debug, Serialize)]
pub struct BriefLogPattern {
    pub node: String,
    pub source: String,
    pub count: u64,
    pub example: String,
}

#[derive(Debug, Serialize)]
pub struct BriefDatabaseFile {
    pub node: String,
    pub file: String,
    pub read_wait_ms: f64,
    pub write_wait_ms: f64,
    pub read_mib: f64,
    pub write_mib: f64,
}

#[derive(Debug, Serialize)]
pub struct BriefDatabaseMemory {
    pub node: String,
    pub buffer_pool_mib: f64,
    pub tables_mib: f64,
}

/// journaldの`RateLimitBurst`の既定値。
const JOURNAL_RATE_LIMIT_BURST: f64 = 10_000.0;

/// `app-log-delta`と`mysql-io-delta`のmetric（`log.`・`db.file.`・`db.memory.`）から、
/// `logs`・`database_io`・`database_memory`の欄を作る。
pub fn attach_logs_and_database_io(brief: &mut BriefOutput, metrics: &[Metric], limit: usize) {
    const MIB: f64 = 1024.0 * 1024.0;
    let label = |metric: &Metric, name: &str| metric.labels.get(name).cloned().unwrap_or_default();
    let short = brief.run.clone();

    let mut logs = Vec::new();
    for metric in metrics {
        let (node, source) = (label(metric, "node"), label(metric, "source"));
        match metric.name.as_str() {
            "log.error_lines" => logs.push(BriefLogPattern {
                node,
                source,
                count: metric.value as u64,
                example: label(metric, "example"),
            }),
            "log.error_lines_omitted" => brief.warnings.push(format!(
                "logs: {} more error lines from {source} on {node} were outside its 10 most frequent patterns",
                metric.value
            )),
            // journaldは1 unitあたり既定で30秒に10000行までしか残さない。捨てたことを知らせる
            // `Suppressed`の行は区間が明けて次の行が来たときに出るので、runの終わりには数えられない。
            "log.lines" if metric.value >= JOURNAL_RATE_LIMIT_BURST && source != "nginx-error" => {
                brief.warnings.push(format!(
                    "logs: {source} on {node} wrote {} lines; journald keeps about {JOURNAL_RATE_LIMIT_BURST} lines per 30s per unit by default, so its error counts may be low",
                    metric.value
                ))
            }
            _ => {}
        }
    }
    logs.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.node.cmp(&b.node)));
    brief.logs = section(logs, limit).more(|| {
        format!("isuscope query {short} --metric log.error_lines --group-by source --group-by pattern --limit 20")
    });

    let mut files = BTreeMap::<(String, String), BriefDatabaseFile>::new();
    let mut memory = BTreeMap::<String, BriefDatabaseMemory>::new();
    for metric in metrics {
        let node = label(metric, "node");
        if let Some(field) = metric.name.strip_prefix("db.file.") {
            let file = label(metric, "file");
            let row = files
                .entry((node.clone(), file.clone()))
                .or_insert_with(|| BriefDatabaseFile {
                    node,
                    file,
                    read_wait_ms: 0.0,
                    write_wait_ms: 0.0,
                    read_mib: 0.0,
                    write_mib: 0.0,
                });
            match field {
                "read_wait" => row.read_wait_ms += metric.value,
                "write_wait" => row.write_wait_ms += metric.value,
                "read_bytes" => row.read_mib += metric.value / MIB,
                "write_bytes" => row.write_mib += metric.value / MIB,
                _ => {}
            }
        } else if let Some(field) = metric.name.strip_prefix("db.memory.") {
            let row = memory
                .entry(node.clone())
                .or_insert_with(|| BriefDatabaseMemory {
                    node,
                    buffer_pool_mib: 0.0,
                    tables_mib: 0.0,
                });
            match field {
                "buffer_pool" => row.buffer_pool_mib = metric.value / MIB,
                "tables" => row.tables_mib = metric.value / MIB,
                _ => {}
            }
        }
    }
    let mut files = files
        .into_values()
        .filter(|row| row.read_wait_ms + row.write_wait_ms > 0.0)
        .collect::<Vec<_>>();
    files.sort_by(|a, b| {
        (b.read_wait_ms + b.write_wait_ms).total_cmp(&(a.read_wait_ms + a.write_wait_ms))
    });
    brief.database_io = section(files, limit).more(|| {
        format!(
            "isuscope query {short} --metric db.file.read_wait --metric db.file.write_wait --group-by file --limit 20"
        )
    });
    brief.database_memory = section(memory.into_values().collect(), usize::MAX);
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
    let (coverage_issues, coverage_notes_hidden) = coverage_issues(diagnostics.coverage);
    let mut http = diagnostics.http;
    http.iter_mut().for_each(query::round_http_summary);
    let (mut database, database_rows_filtered_out, database_window) =
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
    let (hosts, quiet_hosts) = split_quiet_hosts(host_nodes(&diagnostics.host));
    let benchmark_messages = benchmark_messages(&run);
    let mut warnings = benchmark.warnings;
    if let Some(legacy) = diagnostics
        .transition_order
        .legacy_events
        .filter(|count| *count > 0.0)
    {
        warnings.push(format!(
            "{legacy} transition requests had no msec/reqtime in the access log; they are ordered by completion second and counted in ambiguous_count"
        ));
    }
    let profiles_unavailable = diagnostics
        .artifacts
        .iter()
        .filter(|item| item.status == "unavailable")
        .count();
    let short = crate::runner::short_id(&run.id).to_owned();
    let short = short.as_str();
    let database_window_flag = window_flag(database_window.as_deref());
    BriefOutput {
        schema_version: crate::model::OUTPUT_SCHEMA_VERSION,
        run: short.to_owned(),
        review: None,
        summary: BriefRun {
            short_id: crate::runner::short_id(&run.id).into(),
            id: run.id,
            started_at: crate::model::display_time(run.started_at),
            finished_at: run.finished_at.map(crate::model::display_time),
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
        coverage_issues: section(coverage_issues, limit).more(|| collector_failures(short)),
        coverage_notes_hidden,
        benchmark: section(by_metric_then_magnitude(benchmark.rows), limit)
            .more(|| format!("isuscope query {short} --metric-prefix benchmark. --limit 20")),
        score_inputs: section(score_inputs.rows, limit)
            .more(|| format!("isuscope query {short} --metric-prefix score. --limit 20")),
        benchmark_messages,
        logs: section(Vec::new(), limit),
        http: section(http, limit)
            .more(|| format!("isuscope query {short} --view http --limit 20")),
        database: section(database, limit)
            .window(database_window)
            .more(|| {
                format!("isuscope query {short} --view database{database_window_flag} --limit 20")
            }),
        database_rows_filtered_out,
        database_io: section(Vec::new(), limit),
        database_memory: section(Vec::new(), usize::MAX),
        cpu: section(
            diagnostics
                .cpu
                .into_iter()
                .filter(|row| !is_idle(row))
                .collect(),
            limit,
        )
        .more(|| format!("isuscope query {short} --metric cpu.sample_percent --limit 20")),
        hosts: section(hosts, usize::MAX).window(Some(hosts_window.to_owned())),
        quiet_hosts,
        clients: section(clients, usize::MAX).window(Some(hosts_window.to_owned())),
        upstreams: section(diagnostics.upstreams, limit)
            .more(|| format!("isuscope query {short} --metric-prefix http.upstream_ --limit 20")),
        transitions: section(
            transitions(diagnostics.transitions, &diagnostics.transition_order),
            limit,
        )
        .more(|| {
            format!(
                "isuscope sql \"SELECT from_route, to_route, count, p50_ms, p95_ms FROM transitions WHERE run_id LIKE '%{short}' ORDER BY count DESC\""
            )
        }),
        artifact_issues: section(
            diagnostics
                .artifacts
                .into_iter()
                .filter(|item| item.status == "failed")
                .collect(),
            limit,
        )
        .more(|| collector_failures(short)),
        profiles_unavailable,
        next: Vec::new(),
        warnings,
    }
}

fn coverage_issues(coverage: Vec<CoverageSummary>) -> (Vec<CoverageIssueGroup>, usize) {
    type Key = (String, String, String, String, Vec<String>);
    let mut groups = BTreeMap::<Key, (Vec<String>, BTreeSet<String>)>::new();
    let mut info_count = 0;
    for item in coverage {
        let severity = match item.status.as_str() {
            "failed" => "critical",
            "missing" | "partial" => "warning",
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
        || (database.is_empty() && crate::report::supports_database_windows(run, &[]));
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
                connections_in_use_avg: find("client.connections_in_use")
                    .and_then(|row| row.value)
                    .map(|value| query::round_to(value, 1)),
                connections_in_use_max: find("client.connections_in_use")
                    .map(|row| query::round_to(row.peak, 1)),
                // 新規接続数は加算できる値で、`value`は区間の合計（1 bucketぶんではない）。
                connections_opened_per_second: find("client.connections_opened")
                    .and_then(|row| row.value)
                    .zip(window_seconds)
                    .map(|(total, seconds)| query::round_to(total / seconds, 1)),
                request_gap_ms,
                requests_per_connection_avg: find("client.connection_requests_mean")
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
            let busiest_core_max_percent =
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
                    max_percent: query::round_to(row.peak, 2),
                });
            let mut services = rows
                .iter()
                .filter(|row| row.metric == "service.cpu_cores")
                .map(|row| BriefService {
                    service: row.target.clone(),
                    cpu_max_cores: query::round_to(row.peak, 3),
                })
                .collect::<Vec<_>>();
            services.sort_by(|a, b| b.cpu_max_cores.total_cmp(&a.cpu_max_cores));
            services.truncate(3);
            BriefHostNode {
                node: node.into(),
                cpu_busy_avg_percent: average("host.cpu_busy_percent")
                    .or_else(|| average("host.cpu_percent")),
                cpu_busy_max_percent: peak("host.cpu_busy_percent")
                    .or_else(|| peak("host.cpu_percent")),
                busiest_core_max_percent,
                iowait_avg_percent: average("host.cpu_iowait_percent"),
                steal_avg_percent: average("host.cpu_steal_percent"),
                pressure,
                load1_max: peak("host.load1"),
                memory_used_max_mib: peak("host.memory_used_bytes")
                    .map(|bytes| query::round_to(bytes / 1_048_576.0, 1)),
                disk_util_max_percent: peak("host.disk_util_percent"),
                top_services: services,
            }
        })
        .collect()
}

/// idle taskが何もせず待っていたsample。`swapper`でも割り込み処理（softirqなど）は仕事なので残す。
fn is_idle(row: &CpuSummary) -> bool {
    const IDLE_SYMBOLS: &[&str] = &[
        "native_safe_halt",
        "pv_native_safe_halt",
        "default_idle",
        "arch_cpu_idle",
        "cpu_idle_poll",
        "do_idle",
        "poll_idle",
        "intel_idle",
        "intel_idle_irq",
        "acpi_idle_do_entry",
        "acpi_safe_halt",
        "mwait_idle_with_hints",
        "cpuidle_enter_state",
    ];
    row.process.starts_with("swapper") && IDLE_SYMBOLS.contains(&row.symbol.as_str())
}

/// どの値も閾値を下回るnodeを1つにまとめる。実データで遊んでいたnodeはCPUのピークが15%未満、
/// ディスク7%未満、iowait 0.1%未満で、詰まっていたnodeとは桁が違う。CPU全体とコアのピークは必須で、
/// 欠けたnodeは遊んでいたとは言えないので、まとめずにそのまま出す。iowait・PSI・diskは環境によって
/// 取れないので、無ければ判定に使わない（collectorの失敗は`coverage_issues`に出る）。
/// 1台だけならまとめても短くならない。
fn split_quiet_hosts(nodes: Vec<BriefHostNode>) -> (Vec<BriefHostNode>, Option<BriefQuietHosts>) {
    let quiet = |node: &BriefHostNode| {
        node.cpu_busy_max_percent.is_some_and(|value| value < 25.0)
            && node
                .busiest_core_max_percent
                .is_some_and(|value| value < 30.0)
            && node.iowait_avg_percent.is_none_or(|value| value < 5.0)
            && node
                .pressure
                .as_ref()
                .is_none_or(|pressure| pressure.max_percent < 10.0)
            && node.disk_util_max_percent.is_none_or(|value| value < 20.0)
    };
    let (quiet, busy): (Vec<_>, Vec<_>) = nodes.into_iter().partition(quiet);
    if quiet.len() < 2 {
        let mut nodes = busy;
        nodes.extend(quiet);
        nodes.sort_by(|a, b| a.node.cmp(&b.node));
        return (nodes, None);
    }
    let max = |values: &mut dyn Iterator<Item = Option<f64>>| values.flatten().reduce(f64::max);
    let summary = BriefQuietHosts {
        cpu_busy_max_percent: max(&mut quiet.iter().map(|node| node.cpu_busy_max_percent))
            .unwrap_or_default(),
        busiest_core_max_percent: max(&mut quiet.iter().map(|node| node.busiest_core_max_percent))
            .unwrap_or_default(),
        iowait_avg_percent: max(&mut quiet.iter().map(|node| node.iowait_avg_percent)),
        pressure_max_percent: max(&mut quiet
            .iter()
            .map(|node| node.pressure.as_ref().map(|pressure| pressure.max_percent))),
        disk_util_max_percent: max(&mut quiet.iter().map(|node| node.disk_util_max_percent)),
        nodes: quiet.into_iter().map(|node| node.node).collect(),
    };
    (busy, Some(summary))
}

/// briefが要約したDBの区間を`query`でも選ぶ。区間を持たない古いrunでは付けない。
fn window_flag(window: Option<&str>) -> String {
    window
        .map(|window| format!(" --window {window}"))
        .unwrap_or_default()
}

/// 完了しなかったcollectorとそのerrorの全件。
fn collector_failures(short: &str) -> String {
    format!(
        "isuscope sql \"SELECT name, node, phase, status, error FROM collector_runs WHERE run_id LIKE '%{short}' AND status != 'complete'\" --format tsv"
    )
}

/// 比較元があれば、HTTPとDBの比較を次に使えるコマンドとして示す。読むだけのコマンドに限り、
/// どの表を見るべきかのような推測はしない。
pub fn next_steps(brief: &mut BriefOutput) {
    let short = brief.run.clone();
    if let Some(base) = brief
        .review
        .as_ref()
        .and_then(|review| review.comparison.as_ref())
        .map(|comparison| comparison.base.clone())
    {
        brief.next.push(format!(
            "isuscope query {short} --base {base} --view http --limit 20"
        ));
        brief.next.push(format!(
            "isuscope query {short} --base {base} --view database{} --limit 20",
            window_flag(brief.database.window.as_deref())
        ));
    }
}

fn section<T>(mut items: Vec<T>, limit: usize) -> BriefSection<T> {
    let total_count = items.len();
    items.truncate(limit);
    BriefSection {
        total_count,
        truncated: total_count > items.len(),
        window: None,
        more: None,
        items,
        more_command: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_run() -> crate::model::RunManifest {
        serde_json::from_value(serde_json::json!({
            "schema_version": 6, "id": "01a1017e-d070-71c2-a89f-0835b7c5d88e", "mode": "run",
            "state": "complete", "started_at": "2026-09-19T00:00:00Z", "finished_at": null,
            "hypothesis": "受取履歴の全走査を索引で無くし、/loginの遅延と減点を減らす",
            "source": {"repository": ".", "git_available": true,
                "commit_hash": "288ecdfda281d39efa09ca11f32b3c21e9ef9c38", "branch": "main",
                "dirty": false, "state_sha256": "", "untracked": [], "error": null},
            "benchmark": {"mode": "command", "command": [], "exit_code": 0, "score": 5386,
                "passed": true, "messages": [], "error": null},
            "collectors": [], "logs": [], "metric_count": 0, "transition_count": 0
        }))
        .unwrap()
    }

    fn host(node: &str, busy: f64, core: f64, disk: f64) -> BriefHostNode {
        BriefHostNode {
            node: node.into(),
            cpu_busy_avg_percent: Some(busy / 2.0),
            cpu_busy_max_percent: Some(busy),
            busiest_core_max_percent: Some(core),
            iowait_avg_percent: Some(0.1),
            steal_avg_percent: None,
            pressure: Some(BriefPressure {
                resource: "io".into(),
                max_percent: 2.0,
            }),
            load1_max: None,
            memory_used_max_mib: None,
            disk_util_max_percent: Some(disk),
            top_services: Vec::new(),
        }
    }

    #[test]
    fn idle_samples_do_not_take_the_cpu_ranking() {
        let row = |process: &str, symbol: &str| CpuSummary {
            process: process.into(),
            symbol: symbol.into(),
            ..Default::default()
        };
        assert!(is_idle(&row("swapper", "native_safe_halt")));
        // idle taskの中でも割り込み処理は仕事なので順位に残す。
        assert!(!is_idle(&row("swapper", "__softirqentry_text_start")));
        assert!(!is_idle(&row("mysqld", "native_safe_halt")));
    }

    #[test]
    fn quiet_nodes_are_folded_and_nodes_with_missing_values_are_kept() {
        let mut unknown = host("app5", 3.0, 3.0, 1.0);
        unknown.busiest_core_max_percent = None;
        let (hosts, quiet) = split_quiet_hosts(vec![
            host("app1", 60.0, 72.0, 100.0),
            host("app2", 5.5, 5.8, 2.8),
            host("app3", 14.4, 15.7, 6.4),
            unknown,
        ]);
        let names = hosts
            .iter()
            .map(|host| host.node.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["app1", "app5"]);
        let quiet = quiet.unwrap();
        assert_eq!(quiet.nodes, ["app2", "app3"]);
        assert_eq!(quiet.cpu_busy_max_percent, 14.4);
        assert_eq!(quiet.disk_util_max_percent, Some(6.4));
        // 遊んでいたnodeが1台だけなら、まとめずにそのまま出す。
        let (hosts, quiet) = split_quiet_hosts(vec![
            host("app2", 5.0, 5.0, 2.0),
            host("app1", 60.0, 72.0, 100.0),
        ]);
        assert!(quiet.is_none());
        assert_eq!(hosts[0].node, "app1");
    }

    fn review_with(reason: &str, body: &str) -> crate::changes::RunReview {
        let source = crate::model::SourceSnapshot {
            commit_hash: Some("288ecdfda281d39efa09ca11f32b3c21e9ef9c38".into()),
            dirty: true,
            state_sha256: "6c67fe53".repeat(8),
            ..Default::default()
        };
        crate::changes::RunReview {
            latest_analysis: Some(crate::model::RunAnalysis {
                id: "analysis".into(),
                created_at: chrono::Utc::now(),
                verdict: crate::model::AnalysisVerdict::Supported,
                body: body.into(),
                base_run_id: Some("01a10172-4d7a-71a3-80c3-cb2a224dc72b".into()),
            }),
            comparison: None,
            changes: vec![crate::changes::ChangeSummary {
                change: crate::changes::Change {
                    schema_version: 1,
                    id: "receipt-index".into(),
                    description: "受取履歴へ複合索引を張る".into(),
                    created_at: chrono::Utc::now(),
                    target: Some("commit 288ecdfda281".into()),
                },
                latest_decision: Some(crate::changes::Decision {
                    schema_version: 1,
                    id: "decision".into(),
                    change_id: "receipt-index".into(),
                    created_at: chrono::Utc::now(),
                    status: crate::changes::DecisionStatus::Accepted,
                    reason: reason.into(),
                    revisit: None,
                    evidence: vec![crate::changes::Evidence {
                        run_id: "01a1017e-d070-71c2-a89f-0835b7c5d88e".into(),
                        source,
                    }],
                }),
            }],
        }
    }

    #[test]
    fn review_drops_the_repeated_reason_and_cuts_long_text() {
        let body = "減点が減った。".repeat(60);
        let brief = review(review_with(&body, &body));
        let analysis = brief.latest_analysis.unwrap();
        assert_eq!(analysis.base.as_deref(), Some("224dc72b"));
        assert_eq!(analysis.body.chars().count(), EXCERPT_CHARS + 1);
        assert!(analysis.full_text.unwrap().starts_with("isuscope sql"));
        let change = &brief.changes.items[0];
        assert_eq!(change.status, Some("accepted"));
        assert!(change.reason.is_none() && change.reason_same_as_analysis);
        assert_eq!(change.evidence, ["b7c5d88e 288ecdfda281 dirty"]);
        // 分析と違う理由は、短ければそのまま出し、全文への案内も付けない。
        let brief = review(review_with("索引で走査行が減った", &body));
        assert_eq!(
            brief.changes.items[0].reason.as_deref(),
            Some("索引で走査行が減った")
        );
        assert!(!brief.changes.items[0].reason_same_as_analysis);
        assert!(brief.changes.items[0].full_text.is_none());
    }

    /// AIのtool出力の上限（Codexでは約4500 tokens）を超えると、真ん中のsectionが削られる。
    /// 5 nodeで各sectionが埋まった忙しいrunでも、上限に余裕を持って収まる大きさに保つ。
    #[test]
    fn a_busy_five_node_brief_fits_the_tool_output_budget() {
        let nodes = (1..=5)
            .map(|index| format!("practice-12-fifth-20261003-app{index}"))
            .collect::<Vec<_>>();
        let http = (0..18)
            .map(|index| HttpRouteSummary {
                node: nodes[0].clone(),
                method: "POST".into(),
                route: format!("/user/:userId/present/receive/{index}"),
                count: 584.0,
                total_ms: Some(633035.0),
                avg_ms: Some(1083.964),
                min_ms: Some(1.0),
                p50_ms: Some(334.0),
                p95_ms: Some(5458.0),
                p99_ms: Some(6525.0),
                max_ms: Some(10000.0),
                errors: 195.0,
                error_rate: Some(0.333904),
                response_bytes: Some(611933.0),
                status_counts: ["1xx", "2xx", "3xx", "4xx", "5xx"]
                    .into_iter()
                    .map(|status| (status.into(), 100.0))
                    .collect(),
            })
            .collect();
        let database = (0..50)
            .map(|index| DatabaseSummary {
                node: nodes[0].clone(),
                engine: "mysql".into(),
                digest: format!(
                    "SELECT * FROM `user_present_all_received_history` WHERE `user_id`=N AND `present_all_id` IN (N, N, N) AND `deleted_at` IS NULL ORDER BY `created_at` DESC /* {index} */"
                ),
                source: "slp".into(),
                window: Some("load".into()),
                calls: 9512.0,
                total_ms: 861649.247,
                avg_ms: Some(90.585),
                p95_ms: Some(173.887),
                p99_ms: Some(209.11),
                max_ms: Some(1200.5),
                lock_ms: 12.5,
                rows_sent: 9512.0,
                rows_examined: 241836.0,
                rows_examined_per_call: Some(241836.0),
                ..Default::default()
            })
            .collect();
        let cpu = nodes
            .iter()
            .flat_map(|node| {
                std::iter::once(CpuSummary {
                    node: node.clone(),
                    process: "swapper".into(),
                    binary: "[kernel.kallsyms]".into(),
                    symbol: "native_safe_halt".into(),
                    source: "perf-series".into(),
                    sample_percent: 96.9,
                })
                .chain((0..10).map(|index| CpuSummary {
                    node: node.clone(),
                    process: "connection".into(),
                    binary: "mysqld".into(),
                    symbol: format!("btr_cur_search_to_nth_level_{index}"),
                    source: "perf-series".into(),
                    sample_percent: 4.49,
                }))
            })
            .collect();
        let host = nodes
            .iter()
            .enumerate()
            .flat_map(|(index, node)| {
                let busy = if index == 0 { 90.0 } else { 5.0 };
                [
                    ("host.cpu_busy_percent", busy, BTreeMap::new()),
                    (
                        "host.core_busy_percent",
                        busy,
                        BTreeMap::from([("core".into(), "0".into())]),
                    ),
                    ("host.cpu_iowait_percent", busy / 2.0, BTreeMap::new()),
                    ("host.disk_util_percent", busy, BTreeMap::new()),
                    ("host.load1", 2.0, BTreeMap::new()),
                    ("host.memory_used_bytes", 1957359616.0, BTreeMap::new()),
                ]
                .into_iter()
                .map(|(metric, value, labels)| HostSummary {
                    node: node.clone(),
                    metric: metric.into(),
                    target: "host".into(),
                    source: "sysstat".into(),
                    labels,
                    unit: "percent".into(),
                    aggregation: crate::metric_semantics::MetricAggregation::Average,
                    value: Some(value),
                    peak: value,
                    peak_at: None,
                    samples: 20,
                })
                .collect::<Vec<_>>()
            })
            .collect();
        let empty = || {
            query::metric_query(
                "run".into(),
                Vec::new(),
                query::MetricQueryOptions {
                    scope: query::QueryScope::Run,
                    window: None,
                    metrics: Vec::new(),
                    metric_prefix: None,
                    node: None,
                    source: None,
                    labels: Vec::new(),
                    label_contains: Vec::new(),
                    group_by: Vec::new(),
                    limit: usize::MAX,
                },
            )
        };
        let diagnostics = RunDiagnostics {
            run: test_run(),
            coverage: Vec::new(),
            http,
            database,
            cpu,
            host,
            client: Vec::new(),
            host_window: "load",
            upstreams: Vec::new(),
            artifacts: Vec::new(),
            transitions: Vec::new(),
            transition_order: Default::default(),
        };
        let mut brief = build(diagnostics, empty(), empty(), 5);
        let long = "受取履歴の旧単発SELECTは995回・合計373401.675ms。".repeat(20);
        brief.review = Some(review(review_with(&long, &long)));
        assert!(brief.cpu.items.iter().all(|row| row.process != "swapper"));
        assert_eq!(brief.hosts.items.len(), 1);
        let size = serde_json::to_vec(&brief).unwrap().len();
        assert!(size <= 12_000, "brief is {size} bytes");
    }

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
    fn partial_database_coverage_is_visible_even_when_metrics_exist() {
        let error = "2 slow-log records could not be assigned to a window; database aggregation is incomplete";
        let (issues, _) = coverage_issues(vec![CoverageSummary {
            section: "database".into(),
            node: "db1".into(),
            collector: "slp".into(),
            phase: "after".into(),
            status: "partial".into(),
            missing_metrics: vec![],
            error: Some(error.into()),
        }]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, "warning");
        assert_eq!(issues[0].status, "partial");
        assert_eq!(issues[0].errors, [error]);
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

    #[test]
    fn transitions_carry_how_certain_their_order_is() {
        let metric = |name: &str, value: f64, edge: Option<(&str, &str)>| crate::model::Metric {
            name: name.into(),
            value,
            unit: String::new(),
            timestamp: None,
            labels: edge
                .map(|(from, to)| {
                    BTreeMap::from([("from".into(), from.into()), ("to".into(), to.into())])
                })
                .unwrap_or_default(),
        };
        let transition = |from: &str, to: &str| Transition {
            from_route: from.into(),
            to_route: to.into(),
            count: 10,
            p50_ms: Some(5.0),
            p95_ms: Some(9.0),
        };
        let diagnostics = crate::report::diagnose(
            test_run(),
            vec![
                metric("transition.legacy_events", 3.0, None),
                metric("transition.overlap_count", 2.0, Some(("GET /", "GET /a"))),
                metric("transition.ambiguous_count", 0.0, Some(("GET /", "GET /a"))),
            ],
            vec![
                transition("GET /", "GET /a"),
                transition("GET /a", "GET /b"),
            ],
            std::path::PathBuf::from("/nonexistent"),
            None,
        );
        let options = || query::MetricQueryOptions {
            scope: query::QueryScope::Run,
            window: None,
            metrics: Vec::new(),
            metric_prefix: None,
            node: None,
            source: None,
            labels: Vec::new(),
            label_contains: Vec::new(),
            group_by: Vec::new(),
            limit: usize::MAX,
        };
        let empty = || query::metric_query("run".into(), Vec::new(), options());
        let brief = build(diagnostics, empty(), empty(), 5);
        let rows = &brief.transitions.items;
        assert_eq!(rows[0].overlap_count, Some(2.0));
        assert_eq!(rows[0].ambiguous_count, Some(0.0));
        // 順序の記録が無い遷移には付けない。
        assert_eq!(rows[1].overlap_count, None);
        assert!(
            brief
                .warnings
                .iter()
                .any(|warning| warning.starts_with("3 transition requests had no msec")),
            "{:?}",
            brief.warnings
        );
    }

    #[test]
    fn more_is_offered_only_for_a_truncated_section() {
        let cut = section(vec![1, 2, 3], 2).more(|| "isuscope query x".into());
        assert_eq!(cut.more.as_deref(), Some("isuscope query x"));
        let mut whole = section(vec![1, 2], 2).more(|| "isuscope query y".into());
        assert!(whole.more.is_none());
        // briefが出力の上限を超えて後から切った表にも、残りを見るコマンドを出す。
        whole.cut(1);
        assert!(whole.truncated);
        assert_eq!(whole.more.as_deref(), Some("isuscope query y"));
        assert_eq!(window_flag(Some("load")), " --window load");
        assert_eq!(window_flag(None), "");
    }

    #[test]
    fn logs_and_database_io_rank_by_count_and_wait() {
        let diagnostics = crate::report::diagnose(
            test_run(),
            Vec::new(),
            Vec::new(),
            std::path::PathBuf::from("/nonexistent"),
            None,
        );
        let options = || query::MetricQueryOptions {
            scope: query::QueryScope::Run,
            window: None,
            metrics: Vec::new(),
            metric_prefix: None,
            node: None,
            source: None,
            labels: Vec::new(),
            label_contains: Vec::new(),
            group_by: Vec::new(),
            limit: usize::MAX,
        };
        let empty = || query::metric_query("run".into(), Vec::new(), options());
        let mut brief = build(diagnostics, empty(), empty(), 5);
        let metric = |name: &str, value: f64, labels: &[(&str, &str)]| Metric {
            name: name.into(),
            value,
            unit: String::new(),
            timestamp: None,
            labels: labels
                .iter()
                .chain(&[("node", "app1")])
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
        };
        let file = |name: &str, field: &str, value: f64| {
            metric(&format!("db.file.{field}"), value, &[("file", name)])
        };
        attach_logs_and_database_io(
            &mut brief,
            &[
                metric(
                    "log.error_lines",
                    3.0,
                    &[("source", "isu"), ("example", "a")],
                ),
                metric(
                    "log.error_lines",
                    40.0,
                    &[("source", "nginx-error"), ("example", "b")],
                ),
                metric("log.lines", 37_500.0, &[("source", "isu")]),
                metric("log.lines", 41.0, &[("source", "nginx-error")]),
                file("isucon/users.ibd", "read_wait", 2.0),
                file("isucon/user_presents.ibd", "read_wait", 51_100.0),
                file(
                    "isucon/user_presents.ibd",
                    "read_bytes",
                    452.0 * 1_048_576.0,
                ),
                // 待たなかったfileは読み書きがあっても出さない。
                file("undo_001", "write_bytes", 1_048_576.0),
                metric("db.memory.buffer_pool", 134_217_728.0, &[]),
                metric("db.memory.tables", 563_101_696.0, &[]),
            ],
            5,
        );
        let logs = &brief.logs.items;
        assert_eq!(
            (logs[0].source.as_str(), logs[0].count),
            ("nginx-error", 40)
        );
        assert_eq!(logs[1].count, 3);
        // 1万行を超えたunitだけ、journaldが捨てた可能性を知らせる。
        assert_eq!(brief.warnings.len(), 1, "{:?}", brief.warnings);
        assert!(brief.warnings[0].contains("isu on app1 wrote 37500 lines"));
        let files = &brief.database_io.items;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file, "isucon/user_presents.ibd");
        assert_eq!(files[0].read_mib, 452.0);
        let memory = &brief.database_memory.items[0];
        assert_eq!(
            (memory.buffer_pool_mib, memory.tables_mib),
            (128.0, 537.015625)
        );
    }
}
