use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunMode {
    Run,
    DiscoveryRun,
    ScoreRun,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::DiscoveryRun => "discovery-run",
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexContext {
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
    pub codex_context: Option<CodexContext>,
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
