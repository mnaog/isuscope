use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunMode {
    Run,
    #[serde(alias = "discovery-run")]
    SurveyRun,
    /// Legacy persisted value. New runs cannot select this mode.
    ScoreRun,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::SurveyRun => "survey-run",
            Self::ScoreRun => "score-run",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Complete,
    Degraded,
    Failed,
    Aborted,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisStatus {
    #[default]
    NotRequired,
    Pending,
    Complete,
    Skipped,
}

impl AnalysisStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Pending => "pending",
            Self::Complete => "complete",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisVerdict {
    Supported,
    Rejected,
    Inconclusive,
    Skipped,
}

impl AnalysisVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Rejected => "rejected",
            Self::Inconclusive => "inconclusive",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunAnalysis {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub verdict: AnalysisVerdict,
    pub body: String,
    #[serde(default)]
    pub base_run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileDigest {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SourceSnapshot {
    pub repository: String,
    pub git_available: bool,
    pub commit_hash: Option<String>,
    pub branch: Option<String>,
    pub dirty: bool,
    pub state_sha256: String,
    pub untracked: Vec<FileDigest>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolingSnapshot {
    pub isuscope_version: String,
    pub config_sha256: String,
    pub routes_sha256: Option<String>,
    pub setup_script_sha256: Option<String>,
    pub setup_state_sha256: Option<String>,
    #[serde(default)]
    pub extra_files_sha256: BTreeMap<String, String>,
    pub error: Option<String>,
}

fn default_agent() -> String {
    "codex".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContext {
    /// Runs recorded before Claude Code support only linked Codex sessions.
    #[serde(default = "default_agent")]
    pub agent: String,
    pub history_path: String,
    pub session_id: String,
    pub input_id: String,
    pub snapshot_path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BenchmarkResult {
    pub mode: String,
    pub command: Vec<String>,
    pub exit_code: Option<i32>,
    pub score: Option<i64>,
    pub passed: Option<bool>,
    #[serde(default)]
    pub interrupted: bool,
    pub messages: Vec<String>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    pub initialize_started_at: Option<DateTime<Utc>>,
    pub initialize_finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// Organizer-only lines dropped by `[benchmark] operator_line_pattern` before saving.
    #[serde(default)]
    pub operator_lines_dropped: usize,
}

impl BenchmarkResult {
    /// 5秒bucketの区切りの起点。負荷の始まり（initializeの終わり）に揃えると、initializeと
    /// 負荷の両方にまたがるbucketができない。node上のcollector（alp、slp）とperfのparserは
    /// この値から5秒ずつ区切る。initializeの終わりが分からないrunはベンチの始まり。
    pub fn bucket_origin(&self) -> Option<DateTime<Utc>> {
        self.initialize_finished_at
            .or(self.started_at)
            .map(to_micros)
    }

    /// 5秒bucketの区切り（起点と、先頭を切り詰めるベンチの始まり）。
    pub fn bucket_grid(&self) -> BucketGrid {
        BucketGrid {
            origin: self.bucket_origin(),
            begin: self.started_at.map(to_micros),
        }
    }
}

/// 5秒bucketの区切り。`origin`（負荷の始まり）から5秒ずつ区切り、ベンチの始まりをまたぐbucketは
/// 始まりを先頭にした短いbucketにする（先頭が始まりより前だと、wholeで絞ったときに落ちる）。
/// node上のcollector（alp、slp）のawkも同じ区切りを使う。
#[derive(Debug, Clone, Copy, Default)]
pub struct BucketGrid {
    pub origin: Option<DateTime<Utc>>,
    pub begin: Option<DateTime<Utc>>,
}

impl BucketGrid {
    /// `at`を含むbucketの先頭。`origin`が無ければepochの5の倍数で区切る。
    pub fn start(&self, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let start = bucket_start(at, self.origin)?;
        Some(match self.begin {
            Some(begin) if at >= begin && start < begin => begin,
            _ => start,
        })
    }
}

/// `at`を含む5秒bucketの先頭。`origin`があればそこから5秒ずつ、無ければepochの5の倍数で区切る。
pub fn bucket_start(at: DateTime<Utc>, origin: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    const BUCKET_MICROS: i64 = 5_000_000;
    let origin = origin.unwrap_or(DateTime::UNIX_EPOCH);
    let offset = (at - origin).num_microseconds()?;
    Some(origin + chrono::Duration::microseconds(offset.div_euclid(BUCKET_MICROS) * BUCKET_MICROS))
}

/// epoch秒の小数を時刻にする。1.7e9秒台のf64は1e-7秒ほどずれるので、マイクロ秒に丸める
/// （丸めないと、負荷の始まりに揃えたbucketの先頭が始まりより僅かに前になって区間から落ちる）。
pub fn epoch_seconds(value: f64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_micros((value * 1_000_000.0).round() as i64)
}

/// 時刻をマイクロ秒に切り捨てる。区間の境界はcollectorへマイクロ秒で渡し、collectorはそこから
/// bucketを区切る。境界がナノ秒を持ったままだと、bucketの先頭（マイクロ秒）が境界より数百ナノ秒
/// 前になり、そのbucketが丸ごと区間から落ちる。記録・転送・比較をこの精度に揃える。
pub fn to_micros(at: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(at.timestamp_micros()).unwrap_or(at)
}

/// 区間`[start, end)`に入るか。5秒bucketのtimestampはbucketの先頭で、中身は先頭から5秒間なので、
/// 終わりを含めると、次の区間の最初のbucket（負荷の始まりに揃えたもの）までinitializeに入る。
pub fn in_window(at: DateTime<Utc>, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    at >= to_micros(start) && at < to_micros(end)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub value: f64,
    #[serde(default)]
    pub unit: String,
    /// When the value was observed. Aggregate metrics may omit this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fingerprint {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub from_route: String,
    pub to_route: String,
    pub count: i64,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRef {
    pub id: String,
    pub kind: String,
    pub node: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectorResult {
    pub name: String,
    pub node: Option<String>,
    pub phase: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub log_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkMessageKind {
    /// Why the benchmark failed, e.g. the validation error that stopped it.
    Failure,
    /// A representative error the benchmark reported, grouped by `category`.
    Error,
}

impl BenchmarkMessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::Error => "error",
        }
    }
}

/// A benchmark output line kept verbatim by a parser. Metrics carry counts; messages keep
/// the text needed to tell what actually happened (a failed validation, an error target).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkMessage {
    pub kind: BenchmarkMessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichmentResult {
    pub name: String,
    pub status: String,
    pub command: Vec<String>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub log_ids: Vec<String>,
    #[serde(default)]
    pub tooling_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<BenchmarkMessage>,
    /// Messages dropped by the per-kind and per-category sample limits.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_message_count: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl RunManifest {
    /// ベンチ、collector、benchmark parserの結果からrunの状態を決める。初回の確定とenrichの
    /// 両方から呼ぶ（enrichでparserが失敗・回復したら、状態もそれに合わせる）。
    pub fn settle_state(&mut self) {
        let collector_degraded = self
            .collectors
            .iter()
            .any(|collector| collector.status == "failed");
        let enrichment_degraded = self
            .enrichments
            .iter()
            .any(|enrichment| enrichment.status == "failed");
        self.state = match self.benchmark.passed {
            _ if self.benchmark.interrupted => RunState::Aborted,
            Some(true) if collector_degraded || enrichment_degraded => RunState::Degraded,
            Some(true) => RunState::Complete,
            _ => RunState::Failed,
        };
    }

    /// Parser messages of one kind in parser order.
    pub fn benchmark_messages(
        &self,
        kind: BenchmarkMessageKind,
    ) -> impl Iterator<Item = &BenchmarkMessage> {
        self.enrichments
            .iter()
            .flat_map(|enrichment| enrichment.messages.iter())
            .filter(move |message| message.kind == kind)
    }

    /// Why a run did not pass: the parsers' failure lines, or isuscope's own error when the
    /// benchmark produced none (for example the command exited before printing anything).
    pub fn failure_reasons(&self) -> Vec<String> {
        if self.benchmark.passed == Some(true) {
            return Vec::new();
        }
        let parsed = self
            .benchmark_messages(BenchmarkMessageKind::Failure)
            .map(|message| message.text.clone())
            .collect::<Vec<_>>();
        if !parsed.is_empty() {
            return parsed;
        }
        self.benchmark.error.iter().cloned().collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    pub schema_version: u32,
    pub id: String,
    pub mode: RunMode,
    pub state: RunState,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub hypothesis: String,
    #[serde(default)]
    pub analysis_status: AnalysisStatus,
    #[serde(default)]
    pub analyses: Vec<RunAnalysis>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source: SourceSnapshot,
    #[serde(default)]
    pub tooling: ToolingSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "codex_context")]
    pub agent_context: Option<AgentContext>,
    pub benchmark: BenchmarkResult,
    pub collectors: Vec<CollectorResult>,
    #[serde(default)]
    pub enrichments: Vec<EnrichmentResult>,
    pub logs: Vec<LogRef>,
    pub metric_count: usize,
    #[serde(default)]
    pub fingerprint_count: usize,
    pub transition_count: usize,
    /// このrun.jsonが指すstructured snapshotのfile名。enrichは新しい世代を別名で書いてから
    /// run.jsonを書き換えるので、途中で落ちてもrun.jsonと同じ世代のsnapshotが残る。
    /// 無ければ`structured.json.zst`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_snapshot: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::RunMode;

    #[test]
    fn legacy_score_run_mode_remains_deserializable() {
        let mode: RunMode = serde_json::from_str("\"score-run\"").unwrap();
        assert_eq!(mode, RunMode::ScoreRun);
    }

    #[test]
    fn legacy_discovery_run_mode_maps_to_survey_run() {
        let legacy: RunMode = serde_json::from_str("\"discovery-run\"").unwrap();
        assert_eq!(legacy, RunMode::SurveyRun);
        assert_eq!(serde_json::to_string(&legacy).unwrap(), "\"survey-run\"");
    }
}

#[cfg(test)]
mod bucket_tests {
    use super::*;

    #[test]
    fn buckets_start_at_the_load_start_and_are_cut_at_the_benchmark_start() {
        let at = |micros| DateTime::from_timestamp_micros(micros);
        let grid = BucketGrid {
            origin: at(1_012_300_000),
            begin: at(1_000_100_000),
        };
        // 負荷の始まりから5秒ずつ。
        assert_eq!(grid.start(at(1_012_800_000).unwrap()), at(1_012_300_000));
        assert_eq!(grid.start(at(1_012_299_999).unwrap()), at(1_007_300_000));
        // ベンチの始まり（1000.1秒）をまたぐbucket（997.3秒から）は、始まりを先頭にする。
        assert_eq!(grid.start(at(1_001_000_000).unwrap()), at(1_000_100_000));
        // 始まりより前の値は、切り詰めずにそのbucketのまま（区間の外）。
        assert_eq!(grid.start(at(999_000_000).unwrap()), at(997_300_000));
        assert!(in_window(
            at(1_000_100_000).unwrap(),
            at(1_000_100_000).unwrap(),
            at(1_060_000_000).unwrap()
        ));
        assert!(!in_window(
            at(1_060_000_000).unwrap(),
            at(1_000_100_000).unwrap(),
            at(1_060_000_000).unwrap()
        ));
        // 境界がナノ秒を持っていても（1800000000.123456789）、マイクロ秒の先頭のbucketは区間に入る。
        let boundary = DateTime::from_timestamp(1_800_000_000, 123_456_789).unwrap();
        let bucket = DateTime::from_timestamp_micros(1_800_000_000_123_456).unwrap();
        assert!(in_window(
            bucket,
            boundary,
            boundary + chrono::Duration::seconds(60)
        ));
        let benchmark = BenchmarkResult {
            initialize_finished_at: Some(boundary),
            ..Default::default()
        };
        assert_eq!(benchmark.bucket_origin(), Some(bucket));
    }
}
