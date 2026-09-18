//! Change decisions are independent of hypothesis verdicts and deployment state.
//! Immutable JSON records are canonical; SQLite is a rebuildable index.
use crate::{model::SourceSnapshot, storage::Store};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::Path};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Accepted,
    Provisional,
    Rejected,
    Deferred,
}

impl DecisionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Provisional => "provisional",
            Self::Rejected => "rejected",
            Self::Deferred => "deferred",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub schema_version: u32,
    pub id: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub run_id: String,
    pub source: SourceSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub schema_version: u32,
    pub id: String,
    pub change_id: String,
    pub created_at: DateTime<Utc>,
    pub status: DecisionStatus,
    pub reason: String,
    pub revisit: Option<String>,
    pub evidence: Vec<Evidence>,
}

/// A decision validated by [`Store::prepare_change_decision`] and not yet written.
#[derive(Debug)]
pub struct PreparedDecision {
    change_id: String,
    status: DecisionStatus,
    revisit: Option<String>,
    /// Description and target of a change that does not exist yet.
    create: Option<(String, Option<String>)>,
}

#[derive(Debug, Serialize)]
pub struct ChangeHistory {
    pub change: Change,
    pub decisions: Vec<Decision>,
}

#[derive(Debug, Serialize)]
pub struct ChangeSummary {
    pub change: Change,
    pub latest_decision: Option<Decision>,
}

#[derive(Debug, Serialize)]
pub struct RunReview {
    pub latest_analysis: Option<crate::model::RunAnalysis>,
    pub comparison: Option<ScoreComparison>,
    /// Current decisions for changes citing this run, not deployment state.
    pub changes: Vec<ChangeSummary>,
    pub changes_truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct ScoreComparison {
    pub base_run_id: String,
    pub candidate_run_id: String,
    pub score: crate::diff::ScoreDiff,
    pub sample_count_per_side: usize,
    /// 2つのrunが同じ前提で比べられるか。差が出た理由の候補であって、採否の判定ではありません。
    pub conditions: Vec<ComparisonCondition>,
}

/// 比較の前提1つ分。`unknown`を`same`に読み替えないための区別です。
#[derive(Debug, Serialize)]
pub struct ComparisonCondition {
    pub name: &'static str,
    pub state: ConditionState,
    /// 何が違ったか（`changed`のとき）、または何が分からないか（`unknown`のとき）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub detail: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionState {
    Same,
    Changed,
    Unknown,
}

fn condition(
    name: &'static str,
    state: ConditionState,
    detail: Vec<String>,
) -> ComparisonCondition {
    ComparisonCondition {
        name,
        state,
        detail,
    }
}

fn compare_hash(base: &str, candidate: &str) -> ConditionState {
    if base.is_empty() || candidate.is_empty() {
        ConditionState::Unknown
    } else if base == candidate {
        ConditionState::Same
    } else {
        ConditionState::Changed
    }
}

/// 比較の前提を、既に保存しているhashとtagだけから組み立てます。
/// ベンチ側の条件はこちらから観測できないため、`bench:`tagが無ければ`unknown`のままにします。
fn comparison_conditions(
    base: &crate::model::RunManifest,
    candidate: &crate::model::RunManifest,
    base_environment: Option<std::collections::BTreeMap<String, String>>,
    candidate_environment: Option<std::collections::BTreeMap<String, String>>,
) -> Vec<ComparisonCondition> {
    let mut conditions = Vec::new();

    let source_state = compare_hash(&base.source.state_sha256, &candidate.source.state_sha256);
    let mut source_detail = Vec::new();
    if source_state == ConditionState::Changed {
        match (&base.source.commit_hash, &candidate.source.commit_hash) {
            (Some(before), Some(after)) if before != after => {
                source_detail.push(format!(
                    "commit {} -> {}",
                    &before[..12.min(before.len())],
                    &after[..12.min(after.len())]
                ));
            }
            _ => {}
        }
        if base.source.dirty || candidate.source.dirty {
            source_detail.push("uncommitted changes".into());
        }
    }
    conditions.push(condition("source", source_state, source_detail));

    let mut observation_detail = Vec::new();
    let mut observation_state = compare_hash(
        &base.tooling.config_sha256,
        &candidate.tooling.config_sha256,
    );
    if observation_state == ConditionState::Changed {
        observation_detail.push("config.toml".into());
    }
    for (name, before, after) in [
        (
            "routes.toml",
            base.tooling.routes_sha256.clone(),
            candidate.tooling.routes_sha256.clone(),
        ),
        (
            "setup script",
            base.tooling.setup_script_sha256.clone(),
            candidate.tooling.setup_script_sha256.clone(),
        ),
    ] {
        if before != after {
            observation_state = ConditionState::Changed;
            observation_detail.push(name.into());
        }
    }
    for name in base
        .tooling
        .extra_files_sha256
        .keys()
        .chain(candidate.tooling.extra_files_sha256.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        if base.tooling.extra_files_sha256.get(name)
            != candidate.tooling.extra_files_sha256.get(name)
        {
            observation_state = ConditionState::Changed;
            observation_detail.push(name.clone());
        }
    }
    if base.tooling.isuscope_version != candidate.tooling.isuscope_version {
        observation_state = ConditionState::Changed;
        observation_detail.push(format!(
            "isuscope {} -> {}",
            base.tooling.isuscope_version, candidate.tooling.isuscope_version
        ));
    }
    conditions.push(condition(
        "observation",
        observation_state,
        observation_detail,
    ));

    let environment = match (base_environment, candidate_environment) {
        (Some(before), Some(after)) => {
            let mut differences = before
                .keys()
                .chain(after.keys())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .filter(|key| before.get(*key) != after.get(*key))
                .cloned()
                .collect::<Vec<_>>();
            if differences.is_empty() {
                condition("environment", ConditionState::Same, Vec::new())
            } else {
                let total = differences.len();
                differences.truncate(5);
                let shown = differences.len();
                if total > shown {
                    differences.push(format!("and {} more", total - shown));
                }
                condition("environment", ConditionState::Changed, differences)
            }
        }
        _ => condition(
            "environment",
            ConditionState::Unknown,
            vec!["no fingerprints recorded".into()],
        ),
    };
    conditions.push(environment);

    let bench_tag = |run: &crate::model::RunManifest| {
        run.tags
            .iter()
            .find(|tag| tag.starts_with("bench:"))
            .cloned()
    };
    let benchmark = match (bench_tag(base), bench_tag(candidate)) {
        (Some(before), Some(after)) if before == after => {
            condition("benchmark", ConditionState::Same, vec![before])
        }
        (Some(before), Some(after)) => condition(
            "benchmark",
            ConditionState::Changed,
            vec![format!("{before} -> {after}")],
        ),
        _ => condition(
            "benchmark",
            ConditionState::Unknown,
            vec!["tag the runs with `bench:<condition>` to compare".into()],
        ),
    };
    conditions.push(benchmark);
    conditions
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 100
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!("change ID must contain 1–100 ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

// Publish only a complete, synced record, without replacing an existing record.
// Unique temporary names and no-clobber publication also support concurrent writers.
fn publish(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("record has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        fs::hard_link(&temporary, path)
            .with_context(|| format!("cannot publish {}", path.display()))?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

impl Store {
    pub fn run_review(&self, run: &crate::model::RunManifest) -> Result<RunReview> {
        let latest_analysis = run.analyses.last().cloned();
        let comparison = latest_analysis
            .as_ref()
            .and_then(|a| a.base_run_id.as_ref())
            .map(|id| -> Result<_> {
                let base = self.load(id)?;
                let conditions = comparison_conditions(
                    &base,
                    run,
                    self.fingerprint_index(&base.id)?,
                    self.fingerprint_index(&run.id)?,
                );
                Ok(ScoreComparison {
                    base_run_id: id.clone(),
                    candidate_run_id: run.id.clone(),
                    score: crate::diff::score_diff(base.benchmark.score, run.benchmark.score),
                    sample_count_per_side: 1,
                    conditions,
                })
            })
            .transpose()?;
        let mut changes = self.list_changes(None, Some(&run.id), 21)?;
        let changes_truncated = changes.len() > 20;
        changes.truncate(20);
        Ok(RunReview {
            latest_analysis,
            comparison,
            changes,
            changes_truncated,
        })
    }
    pub fn create_change(
        &mut self,
        id: &str,
        description: String,
        target: Option<String>,
    ) -> Result<Change> {
        validate_id(id)?;
        if description.trim().is_empty() {
            bail!("description must not be empty");
        }
        let change = Change {
            schema_version: 1,
            id: id.into(),
            description,
            created_at: Utc::now(),
            target,
        };
        publish(
            &self.data_dir.join("changes").join(id).join("change.json"),
            &change,
        )?;
        self.restore_changes()?;
        Ok(change)
    }

    pub fn change_history(&self, id: &str) -> Result<ChangeHistory> {
        validate_id(id)?;
        let dir = self.data_dir.join("changes").join(id);
        let change: Change = serde_json::from_slice(
            &fs::read(dir.join("change.json"))
                .with_context(|| format!("change '{id}' was not found"))?,
        )?;
        let mut decisions = Vec::new();
        let records = dir.join("decisions");
        if records.is_dir() {
            for entry in fs::read_dir(records)? {
                let path = entry?.path();
                if path.extension().is_some_and(|e| e == "json") {
                    let decision: Decision = serde_json::from_slice(&fs::read(path)?)?;
                    if decision.change_id != id || decision.schema_version != 1 {
                        bail!("invalid decision record");
                    }
                    decisions.push(decision);
                }
            }
        }
        if change.id != id || change.schema_version != 1 {
            bail!("invalid change record");
        }
        decisions.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(ChangeHistory { change, decisions })
    }

    pub fn decide_change(
        &mut self,
        id: &str,
        status: DecisionStatus,
        reason: String,
        revisit: Option<String>,
        runs: Vec<String>,
    ) -> Result<Decision> {
        self.change_history(id)?;
        if reason.trim().is_empty() {
            bail!("reason must not be empty");
        }
        if status == DecisionStatus::Provisional
            && revisit.as_ref().is_none_or(|v| v.trim().is_empty())
        {
            bail!("provisional decisions require a non-empty --revisit");
        }
        if runs.is_empty() {
            bail!("at least one evidence run is required");
        }
        let mut evidence = Vec::new();
        for requested in runs {
            let run_id = self
                .resolve_id(&requested)?
                .with_context(|| format!("run '{requested}' was not found"))?;
            if !self.final_dir(&run_id).is_dir() {
                bail!("evidence run must be finalized");
            }
            if evidence.iter().any(|e: &Evidence| e.run_id == run_id) {
                continue;
            }
            let source = self.load(&run_id)?.source;
            evidence.push(Evidence { run_id, source });
        }
        let decision = Decision {
            schema_version: 1,
            id: Uuid::now_v7().to_string(),
            change_id: id.into(),
            created_at: Utc::now(),
            status,
            reason,
            revisit,
            evidence,
        };
        publish(
            &self
                .data_dir
                .join("changes")
                .join(id)
                .join("decisions")
                .join(format!("{}.json", decision.id)),
            &decision,
        )?;
        self.restore_changes()?;
        Ok(decision)
    }

    /// Checks an `analyze --change --decision` request before the analysis is written.
    /// A missing change is created from `description`, or from the run's hypothesis.
    pub fn prepare_change_decision(
        &self,
        change_id: &str,
        run_id: &str,
        status: DecisionStatus,
        description: Option<String>,
        revisit: Option<String>,
    ) -> Result<PreparedDecision> {
        validate_id(change_id)?;
        if status == DecisionStatus::Provisional
            && revisit.as_ref().is_none_or(|v| v.trim().is_empty())
        {
            bail!("provisional decisions require a non-empty --revisit");
        }
        if !self.final_dir(run_id).is_dir() {
            bail!("evidence run must be finalized");
        }
        let exists = self
            .data_dir
            .join("changes")
            .join(change_id)
            .join("change.json")
            .is_file();
        let create = if exists {
            if description.is_some() {
                bail!(
                    "change '{change_id}' already exists; --description is only for a new change"
                );
            }
            None
        } else {
            let run = self.load(run_id)?;
            let description = description.unwrap_or(run.hypothesis);
            if description.trim().is_empty() {
                bail!("a new change needs --description when the run has no hypothesis");
            }
            let target = run
                .source
                .commit_hash
                .map(|hash| format!("commit {}", &hash[..hash.len().min(12)]));
            Some((description, target))
        };
        Ok(PreparedDecision {
            change_id: change_id.into(),
            status,
            revisit,
            create,
        })
    }

    pub fn record_change_decision(
        &mut self,
        prepared: PreparedDecision,
        reason: String,
        runs: Vec<String>,
    ) -> Result<Decision> {
        if let Some((description, target)) = prepared.create {
            let path = self
                .data_dir
                .join("changes")
                .join(&prepared.change_id)
                .join("change.json");
            // Another writer may have created it since preparation; keep theirs.
            if let Err(error) = self.create_change(&prepared.change_id, description, target)
                && !path.is_file()
            {
                return Err(error);
            }
        }
        self.decide_change(
            &prepared.change_id,
            prepared.status,
            reason,
            prepared.revisit,
            runs,
        )
    }

    pub fn list_changes(
        &self,
        status: Option<DecisionStatus>,
        run: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ChangeSummary>> {
        let mut stmt = self.connection.prepare(
            "SELECT c.id FROM changes c
             WHERE (?1 IS NULL OR (SELECT status FROM change_decisions d WHERE d.change_id=c.id ORDER BY created_at DESC,id DESC LIMIT 1)=?1)
             AND (?2 IS NULL OR EXISTS (SELECT 1 FROM change_decisions d JOIN change_decision_runs r ON r.decision_id=d.id WHERE d.change_id=c.id AND r.run_id=?2))
             ORDER BY c.created_at DESC,c.id LIMIT ?3")?;
        let ids = stmt
            .query_map(
                params![status.map(|s| s.as_str()), run, limit as i64],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| {
                let mut history = self.change_history(&id)?;
                Ok(ChangeSummary {
                    change: history.change,
                    latest_decision: history.decisions.pop(),
                })
            })
            .collect()
    }

    pub(crate) fn restore_changes(&mut self) -> Result<()> {
        let root = self.data_dir.join("changes");
        if !root.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || !entry.path().join("change.json").is_file() {
                continue;
            }
            let history = self.change_history(&entry.file_name().to_string_lossy())?;
            let tx = self.connection.transaction()?;
            let c = history.change;
            tx.execute("INSERT OR IGNORE INTO changes(id,description,created_at,target) VALUES (?1,?2,?3,?4)",
                params![c.id,c.description,c.created_at.to_rfc3339(),c.target])?;
            for d in history.decisions {
                tx.execute("INSERT OR IGNORE INTO change_decisions(id,change_id,created_at,status,reason,revisit) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![d.id,d.change_id,d.created_at.to_rfc3339(),d.status.as_str(),d.reason,d.revisit])?;
                for e in d.evidence {
                    tx.execute("INSERT OR IGNORE INTO change_decision_runs(decision_id,run_id) VALUES (?1,?2)", params![d.id,e.run_id])?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }
}
