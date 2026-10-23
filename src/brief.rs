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
    /// 取れなかったが失敗ではないcollector（`unavailable`：nodeに道具が無いなど）の数。`coverage_issues`には出さない。
    pub coverage_notes_hidden: usize,
    /// `coverage_notes_hidden`の行を読むコマンド。0件なら付かない。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_notes_more: Option<String>,
    pub benchmark: BriefSection<MetricQueryRow>,
    /// Values read from the system under test to work out what the score is made of.
    /// Collected in `survey-run`, where that question is settled.
    pub score_inputs: BriefSection<MetricQueryRow>,
    /// Parser-kept benchmark output lines: why it failed and what the errors were.
    pub benchmark_messages: BriefBenchmarkMessages,
    /// ベンチ前に読んだ接続・fdの上限値（設定のkeyと値）と、同じ上限について観測した値（ピーク、
    /// 上限に当たったことを示すlogの決まった文言、listen overflow）。上限値も観測も無ければ空。
    pub limits: BriefSection<BriefLimit>,
    /// ベンチの間にアプリ（`service_units`）・nginxのerror log・kernelが出したエラーの型。多い順。
    pub logs: BriefSection<BriefLogPattern>,
    /// アプリの状態ログ（`state-log` collector）から、対象がその状態にいた時間の合計が長い順。
    /// 状態ログが無いrunでは空。行は`isuscope states sql`・`states timeline`で読む。
    pub states: BriefSection<BriefState>,
    /// アプリ計測（`app-metrics` collector）の外への呼び出し。呼び先と結果ごとに、負荷区間の
    /// 試行の回数と時間。合計時間の長い順。アプリがまだ書いていなければ空。
    pub app_calls: BriefSection<BriefAppCall>,
    /// アプリ計測のlock。lockごとに、負荷区間の取得の回数、待った時間、持っていた時間。
    /// 待った時間の合計が長い順。
    pub app_locks: BriefSection<BriefAppLock>,
    /// アプリ計測のGoのruntimeの値（`app.go.*`）。nodeごとに、負荷区間のGCのCPUの割合、mutexと
    /// 実行の待ち、割り当て、GC、goroutineの数。Goの書き手が無ければ空。
    pub app_runtime: BriefSection<BriefAppRuntime>,
    /// Goのprofile（`go-profile` collector）の、葉の関数ごとのCPU。`flat_percent`はその関数そのもの、
    /// `cum_percent`は呼んだ先も含めた割合（どちらもそのnodeのGoのprocessのCPUに対して）。
    pub go_cpu: BriefSection<BriefGoCpu>,
    /// 同じCPUを、最も内側のアプリの関数（`main.`）へ寄せたもの。標準libraryやruntimeの中の時間も
    /// それを呼んだアプリの関数に入るので、直す場所がそのまま分かる。
    pub go_cpu_app: BriefSection<BriefGoApp>,
    /// 同じCPUを、アプリが要求に付けた`method`・`route`のlabelで分けたもの。APIごとのCPU。
    pub go_routes: BriefSection<BriefGoRoute>,
    /// GoのprocessのCPUを、葉の関数の種類（GC、割り当て、lock、scheduler、HTTP、暗号、JSON、SQL、
    /// アプリなど）に分けた割合。`source`はprofile（`pprof`）か、profileの無いnodeでのperfのsymbol（`perf`）。
    pub go_classes: BriefSection<BriefGoClass>,
    /// Goのmutexの取り合い（`kind` `mutex`、取り合いになったlockを放したアプリの関数）と、goroutineが
    /// 待った場所（`block`、待ったアプリの関数）。待ちの長い順。
    pub go_waits: BriefSection<BriefGoWait>,
    /// Goの割り当てを、割り当てたアプリの関数ごとに。多い順。
    pub go_allocs: BriefSection<BriefGoAlloc>,
    pub http: BriefSection<HttpRouteSummary>,
    /// 要求数の多いrouteのうち、`http`（合計時間の順）の上位に入らなかったもの。速くて回数の
    /// 多いroute（静的ファイルなど）は合計時間の順では埋もれ、nginxのCPUを食っていても見えない。
    pub http_by_count: BriefSection<HttpRouteSummary>,
    /// 拡張子の付いたpath（`.js`・`.css`・画像など）への要求をまとめた数。静的ファイルは1 fileずつ
    /// 別のrouteなので、1行ずつでは小さく見えても、合わせると全要求の過半を占めることがある。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_static_files: Option<BriefStaticFiles>,
    /// `window`はDBの行を要約した区間（`load`など）。区間を持たない古いrunでは付かない。
    pub database: BriefSection<DatabaseSummary>,
    pub database_rows_filtered_out: usize,
    /// MySQLがfileの読み書きとそれ以外の操作（fsyncなど）で待った時間の長い順。ベンチの前後の差なのでinitializeも含む。
    pub database_io: BriefSection<BriefDatabaseFile>,
    /// buffer poolの大きさと、tableのdataとindexの合計（ベンチ後）。
    pub database_memory: BriefSection<BriefDatabaseMemory>,
    /// idle taskが待機していた時間（`swapper`の`native_safe_halt`など）は除いて順位を付けます。
    /// `sample_percent`は除く前の全sampleに対する割合のままです。
    pub cpu: BriefSection<CpuSummary>,
    /// 1 node 1行で、全nodeを出します。詳細は`query --scope series --window load`で掘ります。
    /// `window`は要約した区間で、initializeの終わりが分かるrunは`load`、分からないrunは`whole`
    /// （initializeを含む）。
    pub hosts: BriefSection<BriefHostNode>,
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
        self.limits.cut(rows);
        self.logs.cut(rows);
        self.states.cut(rows);
        self.app_calls.cut(rows);
        self.app_locks.cut(rows);
        self.go_cpu.cut(rows);
        self.go_cpu_app.cut(rows);
        self.go_routes.cut(rows);
        self.go_classes.cut(rows);
        self.go_waits.cut(rows);
        self.go_allocs.cut(rows);
        self.http.cut(rows);
        // 補助の欄なので、他の表より2行先に減らす。
        self.http_by_count.cut(rows.saturating_sub(2).max(1));
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
    /// インスタンスの型とCPUの型番（ベンチ前のfingerprint）。型の違うnodeが混ざる構成で、
    /// どのnodeが速いかを名前の順ではなく見分ける。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
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
    /// 実行可能なtaskの数のピーク（samplerを含む）。コア数を超えた分はCPUを待っている。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub procs_running_max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_used_max_mib: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_util_max_percent: Option<f64>,
    /// CPUを多く使っていたserviceの上位。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub top_services: Vec<BriefService>,
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
    /// 比較元をどこから取ったか。`analysis`は最新の分析の`--base`、`previous_run`は分析に比較元が
    /// 無いときの、このrunの直前に始まって終わった（ベンチを始めた）run。
    pub base_source: &'static str,
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
        base_source: "analysis",
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

/// `run`より前に始まったrunのうち、終わっていてベンチを始めた最も新しいもの。`prepare_command`で
/// 止まったrunや、同じ構成として断ったrunは比較元にしない。briefと`analyze`の既定の比較元。
pub fn previous_run(
    store: &crate::storage::Store,
    run: &RunManifest,
) -> anyhow::Result<Option<RunManifest>> {
    let mut statement = store.connection.prepare(
        "SELECT id FROM runs WHERE id != ?1 AND state != 'running'
           AND started_at < (SELECT started_at FROM runs WHERE id = ?1)
         ORDER BY started_at DESC LIMIT 200",
    )?;
    let ids = statement
        .query_map([&run.id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let Ok(manifest) = store.load(&id) else {
            continue;
        };
        if manifest.benchmark.mode != "not-started" && manifest.benchmark.started_at.is_some() {
            return Ok(Some(manifest));
        }
    }
    Ok(None)
}

/// 分析に比較元が無いrunでも、直前のrun（[`previous_run`]）とのscoreの差と比較の前提を出す。
/// 比較の前提は分析の比較元と同じ方法で求める（比較元だけを差し替えた写しを`run_review`へ渡す）。
pub fn attach_previous_comparison(
    brief: &mut BriefOutput,
    store: &crate::storage::Store,
) -> anyhow::Result<()> {
    if brief
        .review
        .as_ref()
        .is_some_and(|review| review.comparison.is_some())
    {
        return Ok(());
    }
    let run = store.load(&brief.summary.id)?;
    let Some(base) = previous_run(store, &run)? else {
        return Ok(());
    };
    let mut probe = run;
    probe.analyses = vec![crate::model::RunAnalysis {
        id: String::new(),
        created_at: chrono::Utc::now(),
        verdict: crate::model::AnalysisVerdict::Skipped,
        body: String::new(),
        base_run_id: Some(base.id),
    }];
    let Some(comparison) = store.run_review(&probe)?.comparison else {
        return Ok(());
    };
    let comparison = BriefComparison {
        base: crate::runner::short_id(&comparison.base_run_id).to_owned(),
        base_source: "previous_run",
        score_base: comparison.score.base,
        score: comparison.score.candidate,
        score_delta: comparison.score.delta,
        score_delta_percent: comparison.score.delta_percent,
        conditions: comparison.conditions,
    };
    match brief.review.as_mut() {
        Some(review) => review.comparison = Some(comparison),
        None => {
            brief.review = Some(BriefReview {
                latest_analysis: None,
                comparison: Some(comparison),
                changes: section(Vec::new(), REVIEW_CHANGES),
            })
        }
    }
    Ok(())
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_max_cores: Option<f64>,
    /// processのRSSの合計のピーク（`process-sampler`）。cgroupの`memory.current`とは別の値。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_max_mib: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads_max: Option<f64>,
    /// ベンチの間にMainPIDが変わった回数。クラッシュやOOM killによる再起動の手がかり。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restarts: Option<usize>,
    /// CPUを待っていたthreadの平均の数（runqueueで待った時間÷経過時間、`process-sampler`）。
    /// CPU使用率に余裕があっても、threadの数がコアより多いとここに出る。lockの保持中に
    /// preemptされると、lock待ちもこの分だけ延びる。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_delay_avg_cores: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_delay_max_cores: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct BriefRun {
    /// 出さない。runはトップレベルの`run`（短縮ID）で指す。
    #[serde(skip)]
    pub id: String,
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
    /// ベンチの始まりから終わりまでの秒数。logの行数を時間と並べるのに使い、出力には出さない。
    #[serde(skip)]
    pub benchmark_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Default)]
pub struct BriefBenchmarkMessages {
    pub failure: Vec<String>,
    pub errors: Vec<BriefErrorSamples>,
    /// 失敗にしなかった警告（減点・遅延など）。上限の件数でFAILにするベンチがあるので残す。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<BriefErrorSamples>,
    pub omitted_count: usize,
}

#[derive(Debug, Serialize)]
pub struct BriefErrorSamples {
    pub category: Option<String>,
    pub samples: Vec<String>,
}

fn samples_by_category(run: &RunManifest, kind: BenchmarkMessageKind) -> Vec<BriefErrorSamples> {
    let mut groups = Vec::<BriefErrorSamples>::new();
    for message in run.benchmark_messages(kind) {
        match groups
            .iter_mut()
            .find(|group| group.category == message.category)
        {
            Some(group) => group.samples.push(message.text.clone()),
            None => groups.push(BriefErrorSamples {
                category: message.category.clone(),
                samples: vec![message.text.clone()],
            }),
        }
    }
    groups
}

pub fn benchmark_messages(run: &RunManifest) -> BriefBenchmarkMessages {
    BriefBenchmarkMessages {
        failure: run.failure_reasons(),
        errors: samples_by_category(run, BenchmarkMessageKind::Error),
        warnings: samples_by_category(run, BenchmarkMessageKind::Warning),
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

/// 接続・fdの上限1つ。`config_key`は値を読んだ設定のkey、`value`はベンチ前に読んだ値、`peak`は
/// 同じ上限に数えられる観測値（`peak_metric`）のベンチ中の最大、`evidence`は上限に当たったときに
/// 出る決まった文言のlog行とlisten overflowの数。読めなかった・観測の無い欄は付かない。
#[derive(Debug, Serialize)]
pub struct BriefLimit {
    pub node: String,
    pub config_key: String,
    /// `LimitNOFILE`を読んだunit、またはfdの上限に当たったlogを出したsource。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_metric: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// `limits` collectorが読む上限値のmetricと、その設定のkey。
const LIMIT_KEYS: &[(&str, &str)] = &[
    ("limit.somaxconn", "net.core.somaxconn"),
    ("limit.nginx_worker_connections", "nginx worker_connections"),
    (
        "limit.nginx_worker_rlimit_nofile",
        "nginx worker_rlimit_nofile",
    ),
    ("limit.nginx_keepalive_requests", "nginx keepalive_requests"),
    ("limit.mysql_max_connections", "MySQL max_connections"),
    ("limit.service_nofile", "LimitNOFILE"),
];

/// 上限と同じものを数える観測値（ベンチ中のピークを並べる）。
const LIMIT_PEAKS: &[(&str, &str)] = &[
    ("client.connection_requests_max", "nginx keepalive_requests"),
    ("mysql.threads_connected", "MySQL max_connections"),
];

/// 上限に当たったときにだけ出る、error logの決まった文言（小文字で照合する）と、その上限のkey。
/// fdの上限（EMFILE）はlogのsourceで分ける：nginxなら`worker_rlimit_nofile`、ほかはそのunitの`LimitNOFILE`。
/// 一般的な接続エラー（upstreamの切断、connection refusedなど）は上限を示さないので、ここでは扱わず`logs`に残す。
const LIMIT_SIGNATURES: &[(&[&str], &str)] = &[
    (&["too many open files", "emfile"], "LimitNOFILE"),
    (
        &["worker_connections are not enough"],
        "nginx worker_connections",
    ),
    (&["too many connections"], "MySQL max_connections"),
    (&["possible syn flooding"], "net.core.somaxconn"),
];

/// `limit.`（ベンチ前の上限値）と、同じ上限の観測（接続数のピーク、logの決まった文言、listen
/// overflow）を、nodeと設定のkeyごとに1行へ並べる。上限に近いかどうかの判定はしない。
pub fn attach_limits(brief: &mut BriefOutput, metrics: &[Metric], limit: usize) {
    type Key = LimitKey;
    let label = |metric: &Metric, name: &str| metric.labels.get(name).cloned();
    let mut rows = BTreeMap::<Key, BriefLimit>::new();
    let mut overflows = BTreeMap::<String, f64>::new();
    let mut log_hits = BTreeMap::<Key, (u64, String)>::new();
    for metric in metrics {
        let node = label(metric, "node").unwrap_or_default();
        if let Some((_, key)) = LIMIT_KEYS.iter().find(|(name, _)| *name == metric.name) {
            let service = (metric.name == "limit.service_nofile")
                .then(|| label(metric, "service"))
                .flatten();
            limit_row(&mut rows, (node, key, service)).value = Some(metric.value);
        } else if let Some((name, key)) = LIMIT_PEAKS.iter().find(|(name, _)| *name == metric.name)
        {
            let row = limit_row(&mut rows, (node, key, None));
            row.peak = Some(row.peak.unwrap_or(0.0).max(metric.value));
            row.peak_metric = Some((*name).into());
        } else if metric.name == "host.tcp_listen_overflows" {
            *overflows.entry(node).or_default() += metric.value;
        } else if metric.name == "log.error_lines" {
            let example = label(metric, "example").unwrap_or_default();
            let text = format!("{} {example}", label(metric, "pattern").unwrap_or_default())
                .to_lowercase();
            let Some((_, key)) = LIMIT_SIGNATURES
                .iter()
                .find(|(needles, _)| needles.iter().any(|needle| text.contains(needle)))
            else {
                continue;
            };
            let source = label(metric, "source");
            let key: Key = match (*key, source.as_deref()) {
                ("LimitNOFILE", Some("nginx-error")) => (node, "nginx worker_rlimit_nofile", None),
                ("LimitNOFILE", _) => (node, "LimitNOFILE", source),
                (key, _) => (node, key, None),
            };
            log_hits.entry(key).or_insert((0, example)).0 += metric.value as u64;
        }
    }
    for (node, count) in overflows {
        if count > 0.0 {
            limit_row(&mut rows, (node, "net.core.somaxconn", None)).evidence =
                Some(format!("{count} TCP listen overflows"));
        }
    }
    for (key, (count, example)) in log_hits {
        let row = limit_row(&mut rows, key);
        let hits = format!(
            "{count} log lines: {}",
            crate::model::excerpt(&example, 120).0
        );
        row.evidence = Some(match row.evidence.take() {
            Some(before) => format!("{before}; {hits}"),
            None => hits,
        });
    }
    let mut rows = rows.into_values().collect::<Vec<_>>();
    // 当たった証拠のある行を先に出す。briefの行数で切っても残るように。
    rows.sort_by_key(|row| row.evidence.is_none());
    let short = brief.run.clone();
    brief.limits = section(rows, limit)
        .more(|| format!("isuscope query {short} --metric-prefix limit. --limit 50"));
}

/// limitsの1行の鍵：node、設定のkey、unit。
type LimitKey = (String, &'static str, Option<String>);

fn limit_row(rows: &mut BTreeMap<LimitKey, BriefLimit>, key: LimitKey) -> &mut BriefLimit {
    let (node, config_key, service) = key.clone();
    rows.entry(key).or_insert_with(|| BriefLimit {
        node,
        config_key: config_key.into(),
        service,
        value: None,
        peak: None,
        peak_metric: None,
        evidence: None,
    })
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

/// ある種類の対象がある状態にいた時間。`stays`はその状態を出た回数、`open`は区間の終わりで
/// まだその状態にいた対象の数、`peak`は区間の中で同時にその状態にいた対象の数の最大。
#[derive(Debug, Serialize)]
pub struct BriefState {
    pub node: String,
    pub kind: String,
    pub state: String,
    pub stays: u64,
    pub total_ms: f64,
    pub p95_ms: Option<f64>,
    pub open: u64,
    pub peak: u64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct BriefStaticFiles {
    pub routes: usize,
    pub count: f64,
    pub share_percent: f64,
}

/// pathの最後の部分が`名前.拡張子`（拡張子は英数字で5文字まで）なら静的ファイルとみなす。
fn is_static_file(route: &str) -> bool {
    let path = route.split('?').next().unwrap_or(route);
    let last = path.rsplit('/').next().unwrap_or(path);
    match last.rsplit_once('.') {
        Some((name, extension)) => {
            !name.is_empty()
                && !extension.is_empty()
                && extension.len() <= 5
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
                && !extension.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

fn static_files(http: &[HttpRouteSummary]) -> Option<BriefStaticFiles> {
    let total = http.iter().map(|row| row.count).sum::<f64>();
    let files = http
        .iter()
        .filter(|row| is_static_file(&row.route))
        .collect::<Vec<_>>();
    let count = files.iter().map(|row| row.count).sum::<f64>();
    (total > 0.0 && count > 0.0).then(|| BriefStaticFiles {
        routes: files.len(),
        count,
        share_percent: (count / total * 1000.0).round() / 10.0,
    })
}

#[derive(Debug, Serialize)]
pub struct BriefAppCall {
    pub node: String,
    pub target: String,
    pub outcome: String,
    pub count: f64,
    /// 同じ呼び先の試行に占める割合。失敗の結果なら、そのまま失敗率になる。
    pub share_percent: Option<f64>,
    pub total_ms: f64,
    pub avg_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct BriefAppLock {
    pub node: String,
    pub lock: String,
    pub count: f64,
    pub wait_ms: f64,
    pub wait_avg_ms: Option<f64>,
    pub wait_max_ms: Option<f64>,
    pub hold_ms: f64,
    pub hold_avg_ms: Option<f64>,
    pub hold_max_ms: Option<f64>,
}

/// アプリ計測の区間。`start`より後、`end`（書き手が1秒ごとに書く時刻なので、最後の1秒を含める）まで。
pub struct AppWindow {
    pub name: &'static str,
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
}

/// `app-metrics-delta`が運んだ1秒ごとの`app.`の値を区間で足し、`app_calls`・`app_locks`欄を作る。
pub fn attach_app(
    brief: &mut BriefOutput,
    metrics: &[Metric],
    window: Option<&AppWindow>,
    limit: usize,
) {
    let short = brief.run.clone();
    let label = |metric: &Metric, name: &str| metric.labels.get(name).cloned().unwrap_or_default();
    let inside = |metric: &Metric| match (window, metric.timestamp) {
        (None, _) => true,
        (Some(window), Some(at)) => {
            at > window.start && at <= window.end + chrono::Duration::seconds(1)
        }
        (Some(_), None) => false,
    };
    let mut calls = BTreeMap::<(String, String, String), [f64; 3]>::new();
    let mut locks = BTreeMap::<(String, String), [f64; 5]>::new();
    for metric in metrics.iter().filter(|metric| inside(metric)) {
        let node = metric
            .labels
            .get("node")
            .cloned()
            .unwrap_or_else(|| "local".into());
        if let Some(field) = metric.name.strip_prefix("app.call.") {
            let index = match field {
                "count" => 0,
                "duration_sum" => 1,
                "duration_max" => 2,
                _ => continue,
            };
            let row = calls
                .entry((node, label(metric, "target"), label(metric, "outcome")))
                .or_default();
            if index == 2 {
                row[2] = row[2].max(metric.value);
            } else {
                row[index] += metric.value;
            }
        } else if let Some(field) = metric.name.strip_prefix("app.lock.") {
            let index = match field {
                "count" => 0,
                "wait_sum" => 1,
                "wait_max" => 2,
                "hold_sum" => 3,
                "hold_max" => 4,
                _ => continue,
            };
            let row = locks.entry((node, label(metric, "lock"))).or_default();
            if index == 2 || index == 4 {
                row[index] = row[index].max(metric.value);
            } else {
                row[index] += metric.value;
            }
        }
    }
    let mut target_counts = BTreeMap::<(String, String), f64>::new();
    for ((node, target, _), values) in &calls {
        *target_counts
            .entry((node.clone(), target.clone()))
            .or_default() += values[0];
    }
    let average = |total: f64, count: f64| (count > 0.0).then(|| query::round_to(total / count, 3));
    let mut call_rows = calls
        .into_iter()
        .map(|((node, target, outcome), [count, total, max])| {
            let all = target_counts
                .get(&(node.clone(), target.clone()))
                .copied()
                .unwrap_or_default();
            BriefAppCall {
                share_percent: (all > 0.0).then(|| (count / all * 1000.0).round() / 10.0),
                node,
                target,
                outcome,
                count,
                total_ms: query::round_to(total, 3),
                avg_ms: average(total, count),
                max_ms: (count > 0.0).then_some(query::round_to(max, 3)),
            }
        })
        .collect::<Vec<_>>();
    call_rows.sort_by(|a, b| {
        b.total_ms
            .total_cmp(&a.total_ms)
            .then(b.count.total_cmp(&a.count))
    });
    let mut lock_rows = locks
        .into_iter()
        .map(
            |((node, lock), [count, wait, wait_max, hold, hold_max])| BriefAppLock {
                node,
                lock,
                count,
                wait_ms: query::round_to(wait, 3),
                wait_avg_ms: average(wait, count),
                wait_max_ms: (count > 0.0).then_some(query::round_to(wait_max, 3)),
                hold_ms: query::round_to(hold, 3),
                hold_avg_ms: average(hold, count),
                hold_max_ms: (count > 0.0).then_some(query::round_to(hold_max, 3)),
            },
        )
        .collect::<Vec<_>>();
    lock_rows.sort_by(|a, b| {
        b.wait_ms
            .total_cmp(&a.wait_ms)
            .then(b.hold_ms.total_cmp(&a.hold_ms))
    });
    let window_name = window.map(|window| window.name.to_owned());
    let flag = window.map_or("whole", |window| window.name);
    // 行の無い欄（アプリ計測の無いrun）には区間を付けない。
    let calls_window = (!call_rows.is_empty())
        .then(|| window_name.clone())
        .flatten();
    let locks_window = (!lock_rows.is_empty())
        .then(|| window_name.clone())
        .flatten();
    brief.app_calls = section(call_rows, limit)
        .window(calls_window)
        .more(|| {
            format!(
                "isuscope query {short} --metric-prefix app.call. --scope series --window {flag} --limit 40"
            )
        });
    brief.app_locks = section(lock_rows, limit)
        .window(locks_window)
        .more(|| {
            format!(
                "isuscope query {short} --metric-prefix app.lock. --scope series --window {flag} --limit 40"
            )
        });
}

/// ベンチ前のfingerprint（`instance.type`・`cpu.model`）を`hosts`のnodeへ付ける。値の分からない
/// node（`unknown`、計測の無いnode）には付けない。型が2種類以上あれば`warnings`で伝える。
pub fn attach_node_facts(
    brief: &mut BriefOutput,
    instance_types: &[(String, String)],
    cpu_models: &[(String, String)],
) {
    let known = |value: &str| {
        let value = value.trim();
        (!value.is_empty() && value != "unknown").then(|| value.to_owned())
    };
    for host in &mut brief.hosts.items {
        host.instance_type = instance_types
            .iter()
            .find(|(node, _)| *node == host.node)
            .and_then(|(_, value)| known(value));
        host.cpu_model = cpu_models
            .iter()
            .find(|(node, _)| *node == host.node)
            .and_then(|(_, value)| known(value));
    }
    let mut types = BTreeMap::<&str, Vec<&str>>::new();
    for host in &brief.hosts.items {
        if let Some(kind) = host.instance_type.as_deref() {
            types.entry(kind).or_default().push(&host.node);
        }
    }
    if types.len() > 1 {
        let listed = types
            .iter()
            .map(|(kind, nodes)| format!("{kind} ({})", nodes.join(", ")))
            .collect::<Vec<_>>()
            .join(" / ");
        brief.warnings.push(format!(
            "hosts: nodes run different instance types: {listed}; compare a role's load across nodes with their types in mind"
        ));
    }
}

#[derive(Debug, Serialize)]
pub struct BriefAppRuntime {
    pub node: String,
    /// runtimeが見積もったCPUのうちGCの割合（GCとアプリのCPUの和に対して）。
    pub gc_cpu_percent: Option<f64>,
    /// アプリ（GC以外のGoのコード）が使ったcoreの数の平均。
    pub user_cpu_cores: Option<f64>,
    /// 全mutex（包んでいないものも含む）の待ちの、1秒あたりの合計。
    pub mutex_wait_ms_per_s: Option<f64>,
    /// 実行できるgoroutineがCPUを待った時間の平均と、各1秒のp99の最大。
    pub sched_wait_avg_us: Option<f64>,
    pub sched_wait_p99_max_ms: Option<f64>,
    pub alloc_mb_per_s: Option<f64>,
    pub gc_per_s: Option<f64>,
    pub gc_pause_max_ms: Option<f64>,
    pub heap_live_max_mb: Option<f64>,
    pub goroutines_max: Option<f64>,
    pub gomaxprocs: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct BriefGoCpu {
    pub node: String,
    pub function: String,
    pub flat_percent: f64,
    pub cum_percent: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct BriefGoApp {
    pub node: String,
    pub function: String,
    pub percent: f64,
    /// profileを取った区間の平均で、何core使ったか。
    pub cores: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct BriefGoRoute {
    pub node: String,
    pub method: String,
    pub route: String,
    pub percent: f64,
    pub cores: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct BriefGoClass {
    pub node: String,
    pub process: String,
    pub source: String,
    pub class: String,
    pub percent: f64,
}

#[derive(Debug, Serialize)]
pub struct BriefGoWait {
    pub node: String,
    pub kind: String,
    pub site: String,
    pub delay_ms: f64,
    pub count: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct BriefGoAlloc {
    pub node: String,
    pub site: String,
    pub mb: f64,
    pub share_percent: Option<f64>,
    pub objects: Option<f64>,
}

/// アプリ計測のGoのruntimeの値（`app.go.*`、1秒ごと）を負荷区間で要約し、`app_runtime`欄を作る。
pub fn attach_app_runtime(brief: &mut BriefOutput, metrics: &[Metric], window: Option<&AppWindow>) {
    let inside = |metric: &Metric| match (window, metric.timestamp) {
        (None, _) => true,
        (Some(window), Some(at)) => {
            at > window.start && at <= window.end + chrono::Duration::seconds(1)
        }
        (Some(_), None) => false,
    };
    #[derive(Default)]
    struct Node {
        sums: BTreeMap<String, f64>,
        maxima: BTreeMap<String, f64>,
        seconds: std::collections::BTreeSet<i64>,
    }
    let mut nodes = BTreeMap::<String, Node>::new();
    for metric in metrics.iter().filter(|metric| inside(metric)) {
        let Some(field) = metric.name.strip_prefix("app.go.") else {
            continue;
        };
        let node = metric
            .labels
            .get("node")
            .cloned()
            .unwrap_or_else(|| "local".into());
        let row = nodes.entry(node).or_default();
        if let Some(at) = metric.timestamp {
            row.seconds.insert(at.timestamp());
        }
        *row.sums.entry(field.to_owned()).or_default() += metric.value;
        let maximum = row.maxima.entry(field.to_owned()).or_insert(f64::MIN);
        *maximum = maximum.max(metric.value);
    }
    let rows = nodes
        .into_iter()
        .filter_map(|(node, row)| {
            let seconds = match window {
                Some(window) => (window.end - window.start).num_milliseconds() as f64 / 1000.0,
                None => row.seconds.len() as f64,
            };
            if row.sums.is_empty() || seconds <= 0.0 {
                return None;
            }
            let sum = |name: &str| row.sums.get(name).copied();
            let maximum = |name: &str| row.maxima.get(name).copied();
            let rate = |name: &str, scale: f64| {
                sum(name).map(|value| query::round_to(value / seconds * scale, 3))
            };
            let gc = sum("gc_cpu_ms").unwrap_or_default();
            let user = sum("user_cpu_ms").unwrap_or_default();
            Some(BriefAppRuntime {
                node,
                gc_cpu_percent: (gc + user > 0.0)
                    .then(|| query::round_to(gc / (gc + user) * 100.0, 1)),
                user_cpu_cores: rate("user_cpu_ms", 0.001),
                mutex_wait_ms_per_s: rate("mutex_wait_ms", 1.0),
                sched_wait_avg_us: match (sum("sched_wait_ms"), sum("sched_waits")) {
                    (Some(total), Some(count)) if count > 0.0 => {
                        Some(query::round_to(total / count * 1000.0, 1))
                    }
                    _ => None,
                },
                sched_wait_p99_max_ms: maximum("sched_wait_p99_ms")
                    .map(|value| query::round_to(value, 3)),
                alloc_mb_per_s: rate("alloc_bytes", 1.0 / 1_048_576.0),
                gc_per_s: rate("gc_cycles", 1.0),
                gc_pause_max_ms: maximum("gc_pause_max_ms").map(|value| query::round_to(value, 3)),
                heap_live_max_mb: maximum("heap_live_bytes")
                    .map(|value| query::round_to(value / 1_048_576.0, 1)),
                goroutines_max: maximum("goroutines"),
                gomaxprocs: maximum("gomaxprocs"),
            })
        })
        .collect::<Vec<_>>();
    let short = brief.run.clone();
    let flag = window.map_or("whole", |window| window.name);
    brief.app_runtime = section(rows, usize::MAX)
        .window(window.map(|window| window.name.to_owned()))
        .more(|| format!("isuscope query {short} --metric-prefix app.go. --scope series --window {flag} --limit 40"));
}

/// Goのprofileの集計（`go.`）と、perfのsymbol別の割合（`cpu.sample_percent`、時刻なし）から、
/// `go_cpu`・`go_cpu_app`・`go_routes`・`go_classes`・`go_waits`・`go_allocs`欄を作る。
pub fn attach_go(brief: &mut BriefOutput, go: &[Metric], perf: &[Metric], limit: usize) {
    let short = brief.run.clone();
    let label = |metric: &Metric, name: &str| metric.labels.get(name).cloned().unwrap_or_default();
    let node_of = |metric: &Metric| {
        metric
            .labels
            .get("node")
            .cloned()
            .unwrap_or_else(|| "local".into())
    };
    let mut totals = BTreeMap::<String, f64>::new();
    let mut windows = BTreeMap::<String, f64>::new();
    let mut cumulative = BTreeMap::<(String, String), f64>::new();
    let mut alloc_totals = BTreeMap::<String, f64>::new();
    for metric in go {
        let node = node_of(metric);
        match metric.name.as_str() {
            "go.cpu.seconds" => *totals.entry(node).or_default() += metric.value,
            "go.cpu.window_seconds" => {
                let window = windows.entry(node).or_default();
                *window = window.max(metric.value);
            }
            "go.cpu.cum_seconds" => {
                *cumulative
                    .entry((node, label(metric, "function")))
                    .or_default() += metric.value
            }
            "go.alloc.bytes" => *alloc_totals.entry(node).or_default() += metric.value,
            _ => {}
        }
    }
    let percent = |node: &str, value: f64| {
        totals
            .get(node)
            .filter(|total| **total > 0.0)
            .map(|total| query::round_to(value / total * 100.0, 1))
    };
    let cores = |node: &str, value: f64| {
        windows
            .get(node)
            .filter(|window| **window > 0.0)
            .map(|window| query::round_to(value / window, 3))
    };
    let by_value = |metrics: &mut Vec<(&Metric, f64)>| {
        metrics.sort_by(|a, b| b.1.total_cmp(&a.1));
    };
    let pick = |name: &str| {
        let mut rows = go
            .iter()
            .filter(|metric| metric.name == name)
            .map(|metric| (metric, metric.value))
            .collect::<Vec<_>>();
        by_value(&mut rows);
        rows
    };
    let flat = pick("go.cpu.flat_seconds")
        .into_iter()
        .filter_map(|(metric, value)| {
            let node = node_of(metric);
            let function = label(metric, "function");
            Some(BriefGoCpu {
                flat_percent: percent(&node, value)?,
                cum_percent: cumulative
                    .get(&(node.clone(), function.clone()))
                    .and_then(|cum| percent(&node, *cum)),
                node,
                function,
            })
        })
        .collect::<Vec<_>>();
    let app = pick("go.cpu.app_seconds")
        .into_iter()
        .filter_map(|(metric, value)| {
            let node = node_of(metric);
            Some(BriefGoApp {
                percent: percent(&node, value)?,
                cores: cores(&node, value),
                function: label(metric, "function"),
                node,
            })
        })
        .collect::<Vec<_>>();
    let routes = pick("go.cpu.route_seconds")
        .into_iter()
        .filter_map(|(metric, value)| {
            let node = node_of(metric);
            Some(BriefGoRoute {
                percent: percent(&node, value)?,
                cores: cores(&node, value),
                method: label(metric, "method"),
                route: label(metric, "route"),
                node,
            })
        })
        .collect::<Vec<_>>();
    let mut classes = pick("go.cpu.class_seconds")
        .into_iter()
        .filter_map(|(metric, value)| {
            let node = node_of(metric);
            Some(BriefGoClass {
                percent: percent(&node, value)?,
                process: String::new(),
                source: "pprof".into(),
                class: label(metric, "class"),
                node,
            })
        })
        .collect::<Vec<_>>();
    classes.extend(perf_go_classes(perf, &totals));
    // 内訳の表なので、nodeとprocessごとに1%以上の種類を、多い順に8つまで並べる。
    classes.retain(|row| row.percent >= 1.0);
    classes.sort_by(|a, b| {
        (&a.node, &a.process, &a.source)
            .cmp(&(&b.node, &b.process, &b.source))
            .then(b.percent.total_cmp(&a.percent))
    });
    let mut kept = BTreeMap::<(String, String, String), usize>::new();
    classes.retain(|row| {
        let count = kept
            .entry((row.node.clone(), row.process.clone(), row.source.clone()))
            .or_default();
        *count += 1;
        *count <= 8
    });
    let mut waits = Vec::new();
    for (kind, delay, count) in [
        ("mutex", "go.mutex.delay_ms", "go.mutex.contentions"),
        ("block", "go.block.delay_ms", "go.block.count"),
    ] {
        let counts = go
            .iter()
            .filter(|metric| metric.name == count)
            .map(|metric| ((node_of(metric), label(metric, "site")), metric.value))
            .collect::<BTreeMap<_, _>>();
        for metric in go.iter().filter(|metric| metric.name == delay) {
            let key = (node_of(metric), label(metric, "site"));
            waits.push(BriefGoWait {
                count: counts.get(&key).copied(),
                node: key.0,
                kind: kind.into(),
                site: key.1,
                delay_ms: query::round_to(metric.value, 3),
            });
        }
    }
    waits.sort_by(|a, b| b.delay_ms.total_cmp(&a.delay_ms));
    let objects = go
        .iter()
        .filter(|metric| metric.name == "go.alloc.objects")
        .map(|metric| ((node_of(metric), label(metric, "site")), metric.value))
        .collect::<BTreeMap<_, _>>();
    let allocs = pick("go.alloc.bytes")
        .into_iter()
        .map(|(metric, value)| {
            let key = (node_of(metric), label(metric, "site"));
            BriefGoAlloc {
                mb: query::round_to(value / 1_048_576.0, 1),
                share_percent: alloc_totals
                    .get(&key.0)
                    .filter(|total| **total > 0.0)
                    .map(|total| query::round_to(value / total * 100.0, 1)),
                objects: objects.get(&key).copied(),
                node: key.0,
                site: key.1,
            }
        })
        .collect::<Vec<_>>();
    brief.go_cpu = section(flat, limit)
        .more(|| format!("isuscope query {short} --metric go.cpu.flat_seconds --limit 40"));
    brief.go_cpu_app = section(app, limit)
        .more(|| format!("isuscope query {short} --metric go.cpu.app_seconds --limit 40"));
    brief.go_routes = section(routes, limit)
        .more(|| format!("isuscope query {short} --metric go.cpu.route_seconds --limit 40"));
    brief.go_classes = section(classes, usize::MAX)
        .more(|| format!("isuscope query {short} --metric go.cpu.class_seconds --limit 40"));
    brief.go_waits = section(waits, limit)
        .more(|| format!("isuscope query {short} --metric-prefix go.mutex. --limit 40"));
    brief.go_allocs = section(allocs, limit)
        .more(|| format!("isuscope query {short} --metric go.alloc.bytes --limit 40"));
}

/// profileの無いnodeでは、perfのsymbol別の割合からGoのprocessのCPUを種類に分ける。Goのprocessは、
/// 同じprocessのsymbolに`runtime.`の関数があるもの。kernelの中は`kernel`、Go以外のlibraryは`other`。
fn perf_go_classes(perf: &[Metric], profiled: &BTreeMap<String, f64>) -> Vec<BriefGoClass> {
    let label = |metric: &Metric, name: &str| metric.labels.get(name).cloned().unwrap_or_default();
    let mut go_binaries = std::collections::BTreeSet::<(String, String, String)>::new();
    for metric in perf
        .iter()
        .filter(|metric| metric.name == "cpu.sample_percent")
    {
        if label(metric, "symbol").starts_with("runtime.") {
            go_binaries.insert((
                label(metric, "node"),
                label(metric, "process"),
                label(metric, "binary"),
            ));
        }
    }
    let mut shares = BTreeMap::<(String, String), BTreeMap<&'static str, f64>>::new();
    for metric in perf
        .iter()
        .filter(|metric| metric.name == "cpu.sample_percent")
    {
        let (node, process, binary) = (
            label(metric, "node"),
            label(metric, "process"),
            label(metric, "binary"),
        );
        if profiled.contains_key(&node)
            || !go_binaries
                .iter()
                .any(|(n, p, _)| *n == node && *p == process)
        {
            continue;
        }
        let class = if binary.contains("kernel") {
            "kernel"
        } else if go_binaries.contains(&(node.clone(), process.clone(), binary)) {
            crate::go_profile::function_class(&label(metric, "symbol"))
        } else {
            "other"
        };
        *shares
            .entry((node, process))
            .or_default()
            .entry(class)
            .or_default() += metric.value;
    }
    let mut rows = Vec::new();
    for ((node, process), classes) in shares {
        let total = classes.values().sum::<f64>();
        if total <= 0.0 {
            continue;
        }
        for (class, value) in classes {
            rows.push(BriefGoClass {
                node: node.clone(),
                process: process.clone(),
                source: "perf".into(),
                class: class.into(),
                percent: query::round_to(value / total * 100.0, 1),
            });
        }
    }
    rows
}

/// `state-log-delta`の集計metric（`state.`）から`states`欄を作り、欠けを`warnings`に出す。
pub fn attach_states(brief: &mut BriefOutput, metrics: &[Metric], limit: usize) {
    let label = |metric: &Metric, name: &str| metric.labels.get(name).cloned().unwrap_or_default();
    let short = brief.run.clone();
    let mut rows = BTreeMap::<(String, String, String), BriefState>::new();
    let mut window = None;
    for metric in metrics.iter().filter(|metric| metric.timestamp.is_none()) {
        // local transportのcollectorはnodeを持たない（`states sql`のnodeと同じく`local`）。
        let node = metric
            .labels
            .get("node")
            .cloned()
            .unwrap_or_else(|| "local".into());
        let field = match metric.name.as_str() {
            "state.dwell.count"
            | "state.dwell.total_duration"
            | "state.dwell.p95_duration"
            | "state.dwell.open"
            | "state.occupancy_max" => metric.name.as_str(),
            "state.dropped" if metric.value > 0.0 => {
                brief.warnings.push(format!(
                    "states: the app on {node} dropped {} state events because its writer queue was full; the state log has gaps",
                    metric.value
                ));
                continue;
            }
            "state.invalid_lines" if metric.value > 0.0 => {
                brief.warnings.push(format!(
                    "states: {} lines of the state log on {node} did not follow the format and were skipped",
                    metric.value
                ));
                continue;
            }
            "state.inconsistent" if metric.value > 0.0 => {
                brief.warnings.push(format!(
                    "states: {} state events on {node} started from a state other than the one the log had reached (lost or reordered lines)",
                    metric.value
                ));
                continue;
            }
            "state.truncated_bytes" if metric.value > 0.0 => {
                brief.warnings.push(format!(
                    "states: {} bytes of the state log on {node} were over the per-run limit and were not collected",
                    metric.value
                ));
                continue;
            }
            _ => continue,
        };
        window = window.or_else(|| metric.labels.get("window").cloned());
        let (kind, state) = (label(metric, "kind"), label(metric, "state"));
        let row = rows
            .entry((node.clone(), kind.clone(), state.clone()))
            .or_insert_with(|| BriefState {
                node,
                kind,
                state,
                stays: 0,
                total_ms: 0.0,
                p95_ms: None,
                open: 0,
                peak: 0,
            });
        match field {
            "state.dwell.count" => row.stays = metric.value as u64,
            "state.dwell.total_duration" => row.total_ms = metric.value,
            "state.dwell.p95_duration" => row.p95_ms = Some(metric.value),
            "state.dwell.open" => row.open = metric.value as u64,
            _ => row.peak = metric.value as u64,
        }
    }
    let mut rows = rows.into_values().collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.total_ms
            .total_cmp(&a.total_ms)
            .then_with(|| b.peak.cmp(&a.peak))
            .then_with(|| (&a.node, &a.kind, &a.state).cmp(&(&b.node, &b.kind, &b.state)))
    });
    brief.states = section(rows, limit).window(window).more(|| {
        format!(
            "isuscope states sql {short} \"SELECT kind, state, COUNT(duration_ms) AS stays, SUM(duration_ms) AS total_ms FROM state_stays GROUP BY 1, 2 ORDER BY total_ms DESC\""
        )
    });
}

#[derive(Debug, Serialize)]
pub struct BriefDatabaseFile {
    pub node: String,
    pub file: String,
    pub read_wait_ms: f64,
    pub write_wait_ms: f64,
    /// 読み書き以外の操作（fsync・open・close・statなど）で待った時間。fsyncだけの時間ではない。
    pub misc_wait_ms: f64,
    pub read_mib: f64,
    pub write_mib: f64,
}

#[derive(Debug, Serialize)]
pub struct BriefDatabaseMemory {
    pub node: String,
    pub buffer_pool_mib: f64,
    pub tables_mib: f64,
}

/// journaldの`RateLimitBurst`の既定値（`RateLimitIntervalSec`の既定30秒あたり、unitごと）。
const JOURNAL_RATE_LIMIT_BURST: f64 = 10_000.0;

/// `app-log-delta`と`mysql-io-delta`のmetric（`log.`・`db.file.`・`db.memory.`）から、
/// `logs`・`database_io`・`database_memory`の欄を作る。`network-allowance-delta`の`net.`は
/// 上限を超えたcounterだけを`warnings`へ出す。
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
            // 行数はrun全体の合計で30秒ごとには分からないので、比べずに事実だけを並べる。run全体で
            // 10000行に届かなければ、どの30秒も届いていない。
            // ENAが上限を超えて捨てた・遅らせたpacket。0でなければ、そのnodeはCPU以外の上限に当たっている。
            "net.allowance_exceeded" if metric.value > 0.0 => brief.warnings.push(format!(
                "network: {} on {node} {} increased by {} during the run (including initialize); the instance network allowance dropped or delayed packets",
                label(metric, "counter"),
                label(metric, "device"),
                metric.value
            )),
            "log.lines" if metric.value >= JOURNAL_RATE_LIMIT_BURST && source != "nginx-error" => {
                let seconds = brief
                    .summary
                    .benchmark_seconds
                    .map(|seconds| format!(" in {seconds:.0}s"))
                    .unwrap_or_default();
                brief.warnings.push(format!(
                    "logs: {source} on {node} wrote {} lines{seconds}; journald's default RateLimitBurst is {JOURNAL_RATE_LIMIT_BURST} lines per 30s per unit",
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
                    misc_wait_ms: 0.0,
                    read_mib: 0.0,
                    write_mib: 0.0,
                });
            match field {
                "read_wait" => row.read_wait_ms += metric.value,
                "write_wait" => row.write_wait_ms += metric.value,
                "misc_wait" => row.misc_wait_ms += metric.value,
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
    let waited = |row: &BriefDatabaseFile| row.read_wait_ms + row.write_wait_ms + row.misc_wait_ms;
    let mut files = files
        .into_values()
        .filter(|row| waited(row) > 0.0)
        .collect::<Vec<_>>();
    files.sort_by(|a, b| waited(b).total_cmp(&waited(a)));
    brief.database_io = section(files, limit).more(|| {
        format!(
            "isuscope query {short} --metric db.file.read_wait --metric db.file.write_wait --metric db.file.misc_wait --group-by file --limit 20"
        )
    });
    brief.database_memory = section(memory.into_values().collect(), usize::MAX);
}

#[derive(Debug, Serialize)]
pub struct CoverageIssueGroup {
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
    let http_by_count = share_of_requests(&mut http, limit);
    let http_static_files = static_files(&http);
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
    let benchmark_seconds = run
        .benchmark
        .started_at
        .zip(run.benchmark.finished_at)
        .map(|(start, end)| (end - start).num_milliseconds() as f64 / 1000.0)
        .filter(|seconds| *seconds > 0.0);
    let hosts = host_nodes(&diagnostics.host);
    let benchmark_messages = benchmark_messages(&run);
    let mut warnings = benchmark.warnings;
    if run.benchmark.mode != "not-started"
        && run.benchmark.started_at.is_some()
        && run.benchmark.initialize_finished_at.is_none()
    {
        warnings.push(
            "this run has no initialize boundary, so hosts, clients and database summarize the whole benchmark including initialize; set [benchmark] initialize markers, or [initialize] method and path in routes.toml".into(),
        );
    }
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
            benchmark_seconds,
        },
        coverage_issues: section(coverage_issues, limit).more(|| collector_failures(short)),
        coverage_notes_more: (coverage_notes_hidden > 0).then(|| {
            format!(
                "isuscope sql \"SELECT name, node, phase, error FROM collector_runs WHERE run_id LIKE '%{short}' AND status = 'unavailable'\" --format tsv"
            )
        }),
        coverage_notes_hidden,
        benchmark: section(by_metric_then_magnitude(benchmark.rows), limit)
            .more(|| format!("isuscope query {short} --metric-prefix benchmark. --limit 20")),
        score_inputs: section(score_inputs.rows, limit)
            .more(|| format!("isuscope query {short} --metric-prefix score. --limit 20")),
        benchmark_messages,
        limits: section(Vec::new(), limit),
        logs: section(Vec::new(), limit),
        states: section(Vec::new(), limit),
        app_calls: section(Vec::new(), limit),
        app_locks: section(Vec::new(), limit),
        app_runtime: section(Vec::new(), usize::MAX),
        go_cpu: section(Vec::new(), limit),
        go_cpu_app: section(Vec::new(), limit),
        go_routes: section(Vec::new(), limit),
        go_classes: section(Vec::new(), limit),
        go_waits: section(Vec::new(), limit),
        go_allocs: section(Vec::new(), limit),
        http: section(http, limit)
            .more(|| format!("isuscope query {short} --view http --limit 20")),
        // 補助の欄なので3行まで。他の表の行数を削ってまで並べない。
        http_by_count: section(http_by_count, limit.min(3))
            .more(|| format!("isuscope query {short} --view http --sort count --limit 20")),
        http_static_files,
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

/// 失敗・欠け・一部だけのcollectorを、同じ状態とmetricの欠けごとにnodeをまとめて返す。
/// `unavailable`（nodeに道具が無いなど、collectorが自分で取れないと答えたもの）は数だけを返す。
fn coverage_issues(coverage: Vec<CoverageSummary>) -> (Vec<CoverageIssueGroup>, usize) {
    type Key = (String, String, String, String, Vec<String>);
    let mut groups = BTreeMap::<Key, (Vec<String>, BTreeSet<String>)>::new();
    let mut unavailable = 0;
    for item in coverage {
        match item.status.as_str() {
            "failed" | "missing" | "partial" => {}
            _ if item.missing_metrics.is_empty() => continue,
            _ => {
                unavailable += 1;
                continue;
            }
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
    // 失敗したcollectorを先に並べる（errorを持つのはそれだけ）。
    issues.sort_by(|a, b| {
        (a.status != "failed")
            .cmp(&(b.status != "failed"))
            .then_with(|| a.section.cmp(&b.section))
            .then_with(|| a.collector.cmp(&b.collector))
    });
    (issues, unavailable)
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
    // performance_schemaのdigestから集計した行があれば、slow log（slp）の行は使わない。同じ文を
    // 2つのsourceで数えて並べないため。
    let database = if database.iter().any(|item| item.source == "mysql-digest") {
        database
            .into_iter()
            .filter(|item| item.source == "mysql-digest")
            .collect()
    } else {
        database
    };
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
            let mut services = BTreeMap::<String, BriefService>::new();
            for row in &rows {
                if !matches!(
                    row.metric.as_str(),
                    "service.cpu_cores"
                        | "service.process_rss_bytes"
                        | "service.process_threads"
                        | "service.process_restarts"
                        | "service.run_delay_cores"
                ) {
                    continue;
                }
                let service = services
                    .entry(row.target.clone())
                    .or_insert_with(|| BriefService {
                        service: row.target.clone(),
                        cpu_max_cores: None,
                        rss_max_mib: None,
                        threads_max: None,
                        restarts: None,
                        run_delay_avg_cores: None,
                        run_delay_max_cores: None,
                    });
                match row.metric.as_str() {
                    "service.cpu_cores" => {
                        service.cpu_max_cores = Some(query::round_to(row.peak, 3))
                    }
                    "service.process_rss_bytes" => {
                        service.rss_max_mib = Some(query::round_to(row.peak / 1_048_576.0, 1))
                    }
                    "service.process_threads" => service.threads_max = Some(row.peak),
                    "service.run_delay_cores" => {
                        service.run_delay_avg_cores =
                            row.value.map(|value| query::round_to(value, 3));
                        service.run_delay_max_cores = Some(query::round_to(row.peak, 3));
                    }
                    _ => service.restarts = row.value.map(|count| count as usize),
                }
            }
            let mut services = services.into_values().collect::<Vec<_>>();
            services.sort_by(|a, b| {
                let key = |service: &BriefService| {
                    (
                        service.cpu_max_cores.unwrap_or(0.0),
                        service.rss_max_mib.unwrap_or(0.0),
                    )
                };
                key(b)
                    .0
                    .total_cmp(&key(a).0)
                    .then(key(b).1.total_cmp(&key(a).1))
            });
            services.truncate(3);
            BriefHostNode {
                node: node.into(),
                instance_type: None,
                cpu_model: None,
                cpu_busy_avg_percent: average("host.cpu_busy_percent")
                    .or_else(|| average("host.cpu_percent")),
                cpu_busy_max_percent: peak("host.cpu_busy_percent")
                    .or_else(|| peak("host.cpu_percent")),
                busiest_core_max_percent,
                iowait_avg_percent: average("host.cpu_iowait_percent"),
                steal_avg_percent: average("host.cpu_steal_percent"),
                pressure,
                load1_max: peak("host.load1"),
                procs_running_max: peak("host.procs_running"),
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

/// 各routeに全要求数に占める割合を付け、回数の上位のうち合計時間の上位`limit`件に入らなかった
/// ものを返す。`http`は合計時間の順のまま。
fn share_of_requests(http: &mut [HttpRouteSummary], limit: usize) -> Vec<HttpRouteSummary> {
    let total = http.iter().map(|row| row.count).sum::<f64>();
    if total > 0.0 {
        for row in http.iter_mut() {
            row.share_percent = Some((row.count / total * 1000.0).round() / 10.0);
        }
    }
    let mut by_count = http.iter().skip(limit).cloned().collect::<Vec<_>>();
    query::sort_http_by_count(&mut by_count);
    by_count.retain(|row| row.count > 0.0);
    by_count
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
                share_percent: None,
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
        assert_eq!(brief.hosts.items.len(), 5);
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
    fn database_prefers_statement_digests_over_the_slow_log() {
        // slow logとperformance_schemaの両方で数えたrunでは、同じ文を2行並べない。
        let row = |source: &str| DatabaseSummary {
            node: "db1".into(),
            engine: "mysql".into(),
            digest: "SELECT * FROM t WHERE id = ?".into(),
            source: source.into(),
            window: Some("load".into()),
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
            "collectors": [], "logs": [], "metric_count": 0, "transition_count": 0
        }))
        .unwrap();
        let (rows, omitted, window) =
            preferred_database(vec![row("slp"), row("mysql-digest")], &run);
        assert_eq!(window.as_deref(), Some("load"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "mysql-digest");
        assert_eq!(omitted, 1);
        let (rows, _, _) = preferred_database(vec![row("slp")], &run);
        assert_eq!(rows[0].source, "slp");
    }

    #[test]
    fn services_show_process_memory_threads_and_restarts_without_cgroup_cpu() {
        // cgroup v1ではservice-samplerが動かず、process-samplerの値だけが来る。
        let row = |metric: &str, target: &str, value: f64, peak: f64| HostSummary {
            node: "app3".into(),
            metric: metric.into(),
            target: target.into(),
            source: "process-sampler".into(),
            labels: BTreeMap::new(),
            unit: String::new(),
            aggregation: crate::metric_semantics::MetricAggregation::Average,
            value: Some(value),
            peak,
            peak_at: None,
            samples: 60,
        };
        let nodes = host_nodes(&[
            row("host.cpu_busy_percent", "host", 50.0, 90.0),
            row(
                "service.process_rss_bytes",
                "core.service",
                600.0 * 1_048_576.0,
                1254.0 * 1_048_576.0,
            ),
            row("service.process_threads", "core.service", 99.0, 228.0),
            row("service.process_restarts", "core.service", 1.0, 1.0),
            row("service.run_delay_cores", "core.service", 0.41234, 1.9),
            row("host.procs_running", "host", 3.0, 11.0),
            row(
                "service.process_rss_bytes",
                "nginx.service",
                20.0 * 1_048_576.0,
                30.0 * 1_048_576.0,
            ),
            row("service.memory_bytes", "core.service", 1.0, 1.0),
        ]);
        let services = &nodes[0].top_services;
        assert_eq!(services.len(), 2);
        let core = &services[0];
        assert_eq!(core.service, "core.service");
        assert_eq!(
            (
                core.cpu_max_cores,
                core.rss_max_mib,
                core.threads_max,
                core.restarts
            ),
            (None, Some(1254.0), Some(228.0), Some(1))
        );
        assert_eq!(
            (core.run_delay_avg_cores, core.run_delay_max_cores),
            (Some(0.412), Some(1.9))
        );
        assert_eq!(nodes[0].procs_running_max, Some(11.0));
        assert_eq!(services[1].restarts, None);
        assert_eq!(services[1].run_delay_avg_cores, None);
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
        assert_eq!(issues[0].status, "partial");
        assert_eq!(issues[0].errors, [error]);
    }

    #[test]
    fn coverage_issues_group_nodes_and_count_unavailable_collectors() {
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
        let (issues, unavailable) = coverage_issues(coverage);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].nodes, ["app2", "app3"]);
        assert_eq!(issues[0].occurrences, 2);
        assert_eq!(unavailable, 1);
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
    fn limits_set_each_configured_value_beside_what_was_observed() {
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
        let metric = |node: &str, name: &str, value: f64, labels: &[(&str, &str)]| Metric {
            name: name.into(),
            value,
            unit: String::new(),
            timestamp: None,
            labels: labels
                .iter()
                .chain(&[("node", node)])
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
        };
        let mut brief = build(diagnostics, empty(), empty(), 20);
        attach_limits(
            &mut brief,
            &[
                metric(
                    "app3",
                    "log.error_lines",
                    37.0,
                    &[
                        ("source", "isuride-rust.service"),
                        ("example", "accept error: Too many open files (os error 24)"),
                    ],
                ),
                metric(
                    "app3",
                    "limit.service_nofile",
                    1024.0,
                    &[("service", "isuride-rust.service")],
                ),
                metric("app1", "host.tcp_listen_overflows", 300.0, &[]),
                metric("app1", "host.tcp_listen_overflows", 211.0, &[]),
                metric("app1", "limit.somaxconn", 4096.0, &[]),
                metric("app1", "client.connection_requests_max", 1000.0, &[]),
                metric("app1", "client.connection_requests_max", 640.0, &[]),
                metric("app1", "limit.nginx_worker_connections", 4096.0, &[]),
                metric("app2", "mysql.threads_connected", 140.0, &[]),
                metric("app2", "limit.mysql_max_connections", 151.0, &[]),
                // Generic connection errors do not name a limit; they stay in `logs`.
                metric(
                    "app2",
                    "log.error_lines",
                    5.0,
                    &[(
                        "example",
                        "upstream prematurely closed connection while reading response header from upstream",
                    )],
                ),
                metric(
                    "app2",
                    "log.error_lines",
                    3.0,
                    &[
                        ("source", "nginx-error"),
                        ("example", "accept4() failed (24: Too many open files)"),
                    ],
                ),
            ],
            20,
        );
        let rows = serde_json::to_value(&brief.limits.items).unwrap();
        let find = |node: &str, key: &str| {
            rows.as_array()
                .unwrap()
                .iter()
                .find(|row| row["node"] == node && row["config_key"] == key)
                .unwrap_or_else(|| panic!("no {key} on {node}: {rows}"))
                .clone()
        };
        let nofile = find("app3", "LimitNOFILE");
        assert_eq!(nofile["service"], "isuride-rust.service");
        assert_eq!(nofile["value"], 1024.0);
        assert!(
            nofile["evidence"]
                .as_str()
                .unwrap()
                .starts_with("37 log lines: accept error")
        );
        let backlog = find("app1", "net.core.somaxconn");
        assert_eq!(backlog["value"], 4096.0);
        assert_eq!(backlog["evidence"], "511 TCP listen overflows");
        // nginxで設定されていないkeepalive_requestsは値を付けず、観測のピークだけを並べる。
        let keepalive = find("app1", "nginx keepalive_requests");
        assert!(keepalive.get("value").is_none());
        assert_eq!(keepalive["peak"], 1000.0);
        assert_eq!(keepalive["peak_metric"], "client.connection_requests_max");
        // 値だけの上限も並べる。ホスト全体のTCP接続数とは比べない。
        let connections = find("app1", "nginx worker_connections");
        assert_eq!(connections["value"], 4096.0);
        assert!(connections.get("peak").is_none() && connections.get("evidence").is_none());
        let mysql = find("app2", "MySQL max_connections");
        assert_eq!(
            (mysql["value"].clone(), mysql["peak"].clone()),
            (151.0.into(), 140.0.into())
        );
        assert!(mysql.get("evidence").is_none());
        let nginx_files = find("app2", "nginx worker_rlimit_nofile");
        assert!(
            nginx_files["evidence"]
                .as_str()
                .unwrap()
                .starts_with("3 log lines")
        );
        assert_eq!(rows.as_array().unwrap().len(), 6, "{rows}");
        // 当たった証拠のある行が先に並ぶ。
        assert!(rows[0].get("evidence").is_some() && rows[3].get("evidence").is_none());
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
                metric(
                    "net.allowance_exceeded",
                    1234.0,
                    &[("device", "ens5"), ("counter", "bw_out_allowance_exceeded")],
                ),
                metric(
                    "net.allowance_exceeded",
                    0.0,
                    &[("device", "ens5"), ("counter", "pps_allowance_exceeded")],
                ),
                file("isucon/users.ibd", "read_wait", 2.0),
                // redo logはcommitごとのfsyncで待つ。読み書きの待ちが無くてもMISCだけで出す。
                file("#innodb_redo", "misc_wait", 9_000.0),
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
        assert_eq!(brief.warnings.len(), 2, "{:?}", brief.warnings);
        let warned = |text: &str| brief.warnings.iter().any(|warning| warning.contains(text));
        assert!(warned("isu on app1 wrote 37500 lines"));
        assert!(warned(
            "default RateLimitBurst is 10000 lines per 30s per unit"
        ));
        // 上限に当たったcounterだけを出す。
        assert!(warned(
            "bw_out_allowance_exceeded on app1 ens5 increased by 1234"
        ));
        let files = &brief.database_io.items;
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].file, "isucon/user_presents.ibd");
        assert_eq!(files[0].read_mib, 452.0);
        assert_eq!(
            (files[1].file.as_str(), files[1].misc_wait_ms),
            ("#innodb_redo", 9_000.0)
        );
        let memory = &brief.database_memory.items[0];
        assert_eq!(
            (memory.buffer_pool_mib, memory.tables_mib),
            (128.0, 537.015625)
        );
    }

    #[test]
    fn fast_frequent_routes_surface_beside_the_total_time_ranking() {
        let route = |name: &str, count: f64, total_ms: f64| HttpRouteSummary {
            node: "app1".into(),
            method: "GET".into(),
            route: name.into(),
            count,
            total_ms: Some(total_ms),
            ..Default::default()
        };
        // Sorted by total time, as report::http_routes returns them.
        let mut http = vec![
            route("/api/notification", 10.0, 70_000.0),
            route("/api/coordinate", 170.0, 16_000.0),
            route("/api/evaluation", 8.0, 2_000.0),
            route("/assets/app.js", 600.0, 600.0),
            route("/assets/app.css", 200.0, 200.0),
            route("/favicon.ico", 12.0, 12.0),
        ];
        let by_count = share_of_requests(&mut http, 2);
        assert_eq!(http[1].share_percent, Some(17.0));
        assert_eq!(http[3].share_percent, Some(60.0));
        assert_eq!(
            by_count
                .iter()
                .map(|row| row.route.as_str())
                .collect::<Vec<_>>(),
            [
                "/assets/app.js",
                "/assets/app.css",
                "/favicon.ico",
                "/api/evaluation"
            ]
        );
        assert_eq!(
            static_files(&http),
            Some(BriefStaticFiles {
                routes: 3,
                count: 812.0,
                share_percent: 81.2,
            })
        );
        assert!(is_static_file("/assets/root-BoSp76zW.css"));
        assert!(!is_static_file("/api/v1.2/items"));
        assert!(!is_static_file("/api/rides/:ride_id"));
        assert!(!is_static_file("/user/1.5"));
        // No requests at all: no share and nothing to list.
        let mut empty = vec![route("/x", 0.0, 0.0)];
        assert!(share_of_requests(&mut empty, 0).is_empty());
        assert_eq!(empty[0].share_percent, None);
    }

    #[test]
    fn app_calls_and_locks_sum_the_load_window_and_rank_by_time() {
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
        let start = chrono::DateTime::parse_from_rfc3339("2026-10-20T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let at = |seconds: i64| start + chrono::Duration::seconds(seconds);
        let metric = |name: &str, value: f64, second: i64, labels: &[(&str, &str)]| Metric {
            name: name.into(),
            value,
            unit: String::new(),
            timestamp: Some(at(second)),
            labels: labels
                .iter()
                .chain(&[("node", "app3")])
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
        };
        let failed = [("target", "payment"), ("outcome", "502")];
        let ok = [("target", "payment"), ("outcome", "204")];
        let store = [("lock", "store")];
        let metrics = [
            // initializeの間（区間の外）は足さない。
            metric("app.call.count", 100.0, 0, &failed),
            metric("app.call.count", 7.0, 2, &failed),
            metric("app.call.duration_sum", 70.0, 2, &failed),
            metric("app.call.duration_max", 30.0, 2, &failed),
            metric("app.call.count", 7.0, 3, &failed),
            metric("app.call.duration_sum", 140.0, 3, &failed),
            metric("app.call.duration_max", 50.0, 3, &failed),
            metric("app.call.count", 6.0, 3, &ok),
            metric("app.call.duration_sum", 30.0, 3, &ok),
            metric("app.call.duration_max", 9.0, 3, &ok),
            metric("app.lock.count", 1000.0, 2, &store),
            metric("app.lock.wait_sum", 400.0, 2, &store),
            metric("app.lock.wait_max", 12.0, 2, &store),
            metric("app.lock.hold_sum", 900.0, 2, &store),
            metric("app.lock.hold_max", 3.0, 2, &store),
            metric("app.lock.count", 10.0, 2, &[("lock", "ids")]),
            metric("app.lock.wait_sum", 0.5, 2, &[("lock", "ids")]),
        ];
        let window = AppWindow {
            name: "load",
            start: at(1),
            end: at(60),
        };
        attach_app(&mut brief, &metrics, Some(&window), 5);
        let calls = &brief.app_calls;
        assert_eq!(calls.window.as_deref(), Some("load"));
        assert_eq!(calls.items.len(), 2);
        let first = &calls.items[0];
        assert_eq!(
            (
                first.outcome.as_str(),
                first.count,
                first.total_ms,
                first.avg_ms,
                first.max_ms
            ),
            ("502", 14.0, 210.0, Some(15.0), Some(50.0))
        );
        assert_eq!(first.share_percent, Some(70.0));
        assert_eq!(calls.items[1].share_percent, Some(30.0));
        let locks = &brief.app_locks;
        assert_eq!(locks.items[0].lock, "store");
        assert_eq!(
            (locks.items[0].wait_avg_ms, locks.items[0].hold_max_ms),
            (Some(0.4), Some(3.0))
        );
        assert_eq!(locks.items[1].lock, "ids");
        assert_eq!(locks.items[1].hold_max_ms, Some(0.0));
    }
}
