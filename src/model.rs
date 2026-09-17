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
