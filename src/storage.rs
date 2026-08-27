use crate::{
    collector,
    enrichment::{EnrichmentOutput, PARSER_LABEL},
    model::{
        AnalysisStatus, AnalysisVerdict, Fingerprint, Metric, RunAnalysis, RunManifest, RunState,
        Transition,
    },
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub struct Store {
    data_dir: PathBuf,
    connection: Connection,
}

const STRUCTURED_SNAPSHOT_SCHEMA: u32 = 1;

/// Canonical copy of the rows stored in SQLite. Logs remain the source evidence,
/// while this parser-independent snapshot makes deleting SQLite lossless.
#[derive(Serialize, Deserialize)]
struct StructuredSnapshot {
    schema_version: u32,
    metrics: Vec<Metric>,
    fingerprints: Vec<Fingerprint>,
    transitions: Vec<Transition>,
}

#[derive(Debug)]
pub struct RunSummary {
    pub id: String,
    pub started_at: String,
    pub commit_hash: Option<String>,
    pub dirty: bool,
    pub mode: String,
    pub state: String,
    pub score: Option<i64>,
    pub passed: Option<bool>,
    pub note: Option<String>,
    pub tags: Vec<String>,
    pub hypothesis: String,
    pub analysis_status: String,
    pub latest_analysis_verdict: Option<String>,
    pub latest_analysis_body: Option<String>,
}

#[derive(Debug)]
pub struct PendingAnalysis {
    pub id: String,
    pub hypothesis: String,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir.join("runs/.incomplete"))?;
        let connection = Connection::open(data_dir.join("isuscope.sqlite3"))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&connection)?;
        let mut store = Self {
            data_dir: data_dir.to_path_buf(),
            connection,
        };
        store.restore_missing_finalized_runs()?;
        Ok(store)
    }

    pub fn staging_dir(&self, id: &str) -> PathBuf {
        self.data_dir.join("runs/.incomplete").join(id)
    }

    pub fn final_dir(&self, id: &str) -> PathBuf {
        self.data_dir.join("runs").join(id)
    }

    pub fn recover_incomplete(&mut self) -> Result<Vec<String>> {
        let incomplete = self.data_dir.join("runs/.incomplete");
        let mut recovered = Vec::new();
        for entry in fs::read_dir(&incomplete)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let staging = entry.path();
            let manifest_path = staging.join("run.json");
            if !manifest_path.is_file() {
                continue;
            }
            let mut manifest: RunManifest = serde_json::from_slice(&fs::read(&manifest_path)?)
                .with_context(|| format!("invalid manifest {}", manifest_path.display()))?;
            manifest.state = RunState::Aborted;
            manifest.finished_at = Some(Utc::now());
            manifest.benchmark.passed = Some(false);
            manifest.benchmark.interrupted = true;
            manifest.benchmark.error =
                Some("recovered after an interrupted isuscope process".into());
            manifest.analysis_status = AnalysisStatus::NotRequired;
            write_manifest(&staging, &manifest)?;
            let final_dir = self.final_dir(&manifest.id);
            if final_dir.exists() {
                bail!("cannot recover {}; final run already exists", manifest.id);
            }
            fs::rename(&staging, &final_dir)?;
            self.connection.execute(
                "INSERT OR IGNORE INTO runs (id, started_at, mode, state, commit_hash, dirty, note, hypothesis, analysis_status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    manifest.id,
                    manifest.started_at.to_rfc3339(),
                    manifest.mode.as_str(),
                    manifest.state.as_str(),
                    manifest.source.commit_hash,
                    manifest.source.dirty,
                    manifest.note,
                    manifest.hypothesis,
                    manifest.analysis_status.as_str(),
                ],
            )?;
            persist_codex_context(&self.connection, &manifest)?;
            self.connection.execute(
                "UPDATE runs SET finished_at=?2, state=?3, score=?4, passed=0, exit_code=?5, analysis_status=?6 WHERE id=?1",
                params![
                    manifest.id,
                    manifest.finished_at.map(|value| value.to_rfc3339()),
                    manifest.state.as_str(),
                    manifest.benchmark.score,
                    manifest.benchmark.exit_code,
                    manifest.analysis_status.as_str(),
                ],
            )?;
            for tag in &manifest.tags {
                self.connection.execute(
                    "INSERT OR IGNORE INTO run_tags (run_id, tag) VALUES (?1, ?2)",
                    params![manifest.id, tag],
                )?;
            }
            recovered.push(manifest.id);
        }
        Ok(recovered)
    }

    pub fn begin(&self, manifest: &RunManifest) -> Result<PathBuf> {
        let staging = self.staging_dir(&manifest.id);
        if staging.join("run.json").exists() {
            bail!(
                "run staging directory already exists: {}",
                staging.display()
            );
        }
        fs::create_dir_all(staging.join("source"))?;
        fs::create_dir_all(staging.join("logs"))?;
        fs::create_dir_all(staging.join("tmp"))?;
        write_manifest(&staging, manifest)?;
        self.connection.execute(
            "INSERT INTO runs (id, started_at, mode, state, commit_hash, dirty, note, hypothesis, analysis_status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                manifest.id,
                manifest.started_at.to_rfc3339(),
                manifest.mode.as_str(),
                manifest.state.as_str(),
                manifest.source.commit_hash,
                manifest.source.dirty,
                manifest.note,
                manifest.hypothesis,
                manifest.analysis_status.as_str(),
            ],
        )?;
        persist_codex_context(&self.connection, manifest)?;
        for tag in &manifest.tags {
            self.connection.execute(
                "INSERT INTO run_tags (run_id, tag) VALUES (?1, ?2)",
                params![manifest.id, tag],
            )?;
        }
        Ok(staging)
    }

    /// Persist in-progress metadata needed by after collectors.
    pub fn checkpoint(&self, manifest: &RunManifest) -> Result<()> {
        write_manifest(&self.staging_dir(&manifest.id), manifest)
    }

    pub fn finish(
        &mut self,
        manifest: &RunManifest,
        metrics: &[Metric],
        fingerprints: &[Fingerprint],
        transitions: &[Transition],
    ) -> Result<PathBuf> {
        let staging = self.staging_dir(&manifest.id);
        write_manifest(&staging, manifest)?;
        write_structured_snapshot(&staging, metrics, fingerprints, transitions)?;
        let final_dir = self.final_dir(&manifest.id);
        fs::rename(&staging, &final_dir)
            .with_context(|| format!("cannot finalize run directory {}", final_dir.display()))?;

        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE runs SET finished_at=?2, state=?3, commit_hash=?4, dirty=?5, score=?6, passed=?7, exit_code=?8, note=?9, hypothesis=?10, analysis_status=?11 WHERE id=?1",
            params![
                manifest.id,
                manifest.finished_at.map(|value| value.to_rfc3339()),
                manifest.state.as_str(),
                manifest.source.commit_hash,
                manifest.source.dirty,
                manifest.benchmark.score,
                manifest.benchmark.passed,
                manifest.benchmark.exit_code,
                manifest.note,
                manifest.hypothesis,
                manifest.analysis_status.as_str(),
            ],
        )?;
        persist_codex_context(&transaction, manifest)?;
        for log in &manifest.logs {
            transaction.execute(
                "INSERT INTO logs (id, run_id, kind, node) VALUES (?1, ?2, ?3, ?4)",
                params![log.id, manifest.id, log.kind, log.node],
            )?;
        }
        for metric in metrics {
            transaction.execute(
                "INSERT INTO metrics (run_id, name, value, unit, observed_at, labels_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![manifest.id, metric.name, metric.value, metric.unit, metric.timestamp.map(|value| value.to_rfc3339()), serde_json::to_string(&metric.labels)?],
            )?;
        }
        for fingerprint in fingerprints {
            transaction.execute(
                "INSERT INTO fingerprints (run_id, name, value, labels_json) VALUES (?1, ?2, ?3, ?4)",
                params![manifest.id, fingerprint.name, fingerprint.value, serde_json::to_string(&fingerprint.labels)?],
            )?;
        }
        for transition in transitions {
            transaction.execute(
                "INSERT INTO transitions (run_id, from_route, to_route, count, p50_ms, p95_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![manifest.id, transition.from_route, transition.to_route, transition.count, transition.p50_ms, transition.p95_ms],
            )?;
        }
        for collector in &manifest.collectors {
            transaction.execute(
                "INSERT INTO collector_runs (run_id, name, node, phase, status, exit_code, error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![manifest.id, collector.name, collector.node, collector.phase, collector.status, collector.exit_code, collector.error],
            )?;
        }
        for enrichment in &manifest.enrichments {
            transaction.execute(
                "INSERT INTO enrichment_runs (run_id, name, status, command_json, exit_code, error, log_ids_json, tooling_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    manifest.id,
                    enrichment.name,
                    enrichment.status,
                    serde_json::to_string(&enrichment.command)?,
                    enrichment.exit_code,
                    enrichment.error,
                    serde_json::to_string(&enrichment.log_ids)?,
                    enrichment.tooling_path,
                ],
            )?;
        }
        for analysis in &manifest.analyses {
            transaction.execute(
                "INSERT INTO run_analyses (id, run_id, created_at, verdict, body) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    analysis.id,
                    manifest.id,
                    analysis.created_at.to_rfc3339(),
                    analysis.verdict.as_str(),
                    analysis.body,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(final_dir)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<RunSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT r.id, r.started_at, r.commit_hash, r.dirty, r.mode, r.state, r.score, r.passed, r.note, r.hypothesis, r.analysis_status,
                    (SELECT a.verdict FROM run_analyses a WHERE a.run_id=r.id ORDER BY a.created_at DESC, a.id DESC LIMIT 1),
                    (SELECT a.body FROM run_analyses a WHERE a.run_id=r.id ORDER BY a.created_at DESC, a.id DESC LIMIT 1)
             FROM runs r ORDER BY r.started_at DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(RunSummary {
                id: row.get(0)?,
                started_at: row.get(1)?,
                commit_hash: row.get(2)?,
                dirty: row.get(3)?,
                mode: row.get(4)?,
                state: row.get(5)?,
                score: row.get(6)?,
                passed: row.get(7)?,
                note: row.get(8)?,
                tags: Vec::new(),
                hypothesis: row.get(9)?,
                analysis_status: row.get(10)?,
                latest_analysis_verdict: row.get(11)?,
                latest_analysis_body: row.get(12)?,
            })
        })?;
        let mut runs = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        for run in &mut runs {
            run.tags = self.tags(&run.id)?;
        }
        Ok(runs)
    }

    pub fn resolve_id(&self, requested: &str) -> Result<Option<String>> {
        if requested == "latest" {
            return self
                .connection
                .query_row(
                    "SELECT id FROM runs ORDER BY started_at DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(Into::into);
        }
        let exact: Option<String> = self
            .connection
            .query_row("SELECT id FROM runs WHERE id=?1", [requested], |row| {
                row.get(0)
            })
            .optional()?;
        if exact.is_some() {
            return Ok(exact);
        }
        let tagged = self
            .connection
            .prepare("SELECT run_id FROM run_tags WHERE tag=?1 ORDER BY rowid DESC")?
            .query_map([requested], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match tagged.len() {
            1 => return Ok(tagged.into_iter().next()),
            count if count > 1 => bail!("run tag `{requested}` matches {count} runs"),
            _ => {}
        }
        let mut statement = self
            .connection
            .prepare("SELECT id FROM runs WHERE id LIKE ?1 || '%' OR id LIKE '%' || ?1")?;
        let matches = statement
            .query_map([requested], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            _ => bail!("run ID prefix `{requested}` is ambiguous"),
        }
    }

    pub fn load(&self, id: &str) -> Result<RunManifest> {
        let final_path = self.final_dir(id).join("run.json");
        let path = if final_path.is_file() {
            final_path
        } else {
            self.staging_dir(id).join("run.json")
        };
        let raw = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_slice(&raw).context("invalid run manifest")
    }

    pub fn metrics(&self, id: &str) -> Result<Vec<Metric>> {
        let mut statement = self.connection.prepare(
            "SELECT name, value, unit, observed_at, labels_json FROM metrics WHERE run_id=?1 ORDER BY COALESCE(observed_at, ''), id",
        )?;
        let rows = statement.query_map([id], |row| {
            let labels: String = row.get(4)?;
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get::<_, Option<String>>(3)?,
                labels,
            ))
        })?;
        rows.map(|row| {
            let (name, value, unit, timestamp, labels_json) = row?;
            Ok(Metric {
                name,
                value,
                unit,
                timestamp: timestamp
                    .map(|value| value.parse())
                    .transpose()
                    .context("invalid metric timestamp")?,
                labels: serde_json::from_str(&labels_json).context("invalid metric labels")?,
            })
        })
        .collect()
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn tags(&self, id: &str) -> Result<Vec<String>> {
        let mut statement = self
            .connection
            .prepare("SELECT tag FROM run_tags WHERE run_id=?1 ORDER BY tag")?;
        statement
            .query_map([id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn pending_analyses(&self) -> Result<Vec<PendingAnalysis>> {
        let mut statement = self.connection.prepare(
            "SELECT id, hypothesis FROM runs WHERE passed=1 AND analysis_status='pending' ORDER BY started_at",
        )?;
        statement
            .query_map([], |row| {
                Ok(PendingAnalysis {
                    id: row.get(0)?,
                    hypothesis: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn append_analysis(
        &mut self,
        id: &str,
        verdict: AnalysisVerdict,
        body: String,
    ) -> Result<RunManifest> {
        if !self.final_dir(id).is_dir() {
            bail!("run `{id}` is not finalized");
        }
        if body.trim().is_empty() {
            bail!("analysis body must not be empty");
        }
        let mut manifest = self.load(id)?;
        if manifest.benchmark.passed != Some(true) {
            bail!("only a passing run can be analyzed: `{id}`");
        }
        if verdict == AnalysisVerdict::Skipped
            && manifest.analysis_status != AnalysisStatus::Pending
        {
            bail!("only an analysis-pending run can be skipped: `{id}`");
        }
        let analysis = RunAnalysis {
            id: Uuid::now_v7().to_string(),
            created_at: Utc::now(),
            verdict,
            body,
        };
        manifest.analysis_status = if verdict == AnalysisVerdict::Skipped {
            AnalysisStatus::Skipped
        } else {
            AnalysisStatus::Complete
        };
        manifest.analyses.push(analysis.clone());
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO run_analyses (id, run_id, created_at, verdict, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                analysis.id,
                manifest.id,
                analysis.created_at.to_rfc3339(),
                analysis.verdict.as_str(),
                analysis.body,
            ],
        )?;
        transaction.execute(
            "UPDATE runs SET analysis_status=?2 WHERE id=?1",
            params![manifest.id, manifest.analysis_status.as_str()],
        )?;
        transaction.commit()?;
        write_manifest(&self.final_dir(id), &manifest)?;
        Ok(manifest)
    }

    pub fn annotate(
        &mut self,
        id: &str,
        note: Option<String>,
        tags: &[String],
        remove_tags: &[String],
    ) -> Result<RunManifest> {
        if !self.final_dir(id).is_dir() {
            bail!("run `{id}` is not finalized");
        }
        let mut manifest = self.load(id)?;
        if let Some(note) = note {
            manifest.note = (!note.trim().is_empty()).then_some(note);
        }
        for tag in tags {
            let tag = tag.trim();
            if !tag.is_empty() && !manifest.tags.iter().any(|value| value == tag) {
                manifest.tags.push(tag.to_owned());
            }
        }
        manifest
            .tags
            .retain(|tag| !remove_tags.iter().any(|removed| removed == tag));
        manifest.tags.sort();
        manifest.tags.dedup();
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE runs SET note=?2 WHERE id=?1",
            params![id, manifest.note],
        )?;
        transaction.execute("DELETE FROM run_tags WHERE run_id=?1", [id])?;
        for tag in &manifest.tags {
            transaction.execute(
                "INSERT INTO run_tags (run_id, tag) VALUES (?1, ?2)",
                params![id, tag],
            )?;
        }
        transaction.commit()?;
        write_manifest(&self.final_dir(id), &manifest)?;
        Ok(manifest)
    }

    pub fn replace_enrichments(
        &mut self,
        manifest: &mut RunManifest,
        outputs: Vec<EnrichmentOutput>,
    ) -> Result<()> {
        let mut names = outputs
            .iter()
            .map(|output| output.result.name.clone())
            .collect::<Vec<_>>();
        names.extend(
            manifest
                .enrichments
                .iter()
                .map(|result| result.name.clone()),
        );
        names.sort();
        names.dedup();
        manifest.enrichments.clear();
        manifest
            .logs
            .retain(|log| !log.kind.starts_with("benchmark-parser:"));
        for output in &outputs {
            manifest.enrichments.push(output.result.clone());
            manifest.logs.extend(output.logs.clone());
        }

        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM enrichment_runs WHERE run_id=?1",
            [&manifest.id],
        )?;
        for name in &names {
            transaction.execute(
                "DELETE FROM metrics WHERE run_id=?1 AND json_extract(labels_json, '$.\"isuscope.parser\"')=?2",
                params![manifest.id, name],
            )?;
            transaction.execute(
                "DELETE FROM logs WHERE run_id=?1 AND kind LIKE ?2",
                params![manifest.id, format!("benchmark-parser:{name}:%")],
            )?;
        }
        for output in &outputs {
            for log in &output.logs {
                transaction.execute(
                    "INSERT OR REPLACE INTO logs (id, run_id, kind, node) VALUES (?1, ?2, ?3, ?4)",
                    params![log.id, manifest.id, log.kind, log.node],
                )?;
            }
            for metric in &output.metrics {
                transaction.execute(
                    "INSERT INTO metrics (run_id, name, value, unit, observed_at, labels_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![manifest.id, metric.name, metric.value, metric.unit, metric.timestamp.map(|value| value.to_rfc3339()), serde_json::to_string(&metric.labels)?],
                )?;
            }
            transaction.execute(
                "INSERT OR REPLACE INTO enrichment_runs (run_id, name, status, command_json, exit_code, error, log_ids_json, tooling_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    manifest.id,
                    output.result.name,
                    output.result.status,
                    serde_json::to_string(&output.result.command)?,
                    output.result.exit_code,
                    output.result.error,
                    serde_json::to_string(&output.result.log_ids)?,
                    output.result.tooling_path,
                ],
            )?;
        }
        let metric_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM metrics WHERE run_id=?1",
            [&manifest.id],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        manifest.metric_count = metric_count as usize;
        // Keep the recovery snapshot in lockstep with parser enrichment. This
        // deliberately happens before run.json so a crash cannot advertise a
        // metric count that its snapshot does not contain.
        let run_dir = self.final_dir(&manifest.id);
        let mut snapshot = match read_structured_snapshot(&run_dir)? {
            Some(snapshot) => snapshot,
            None => self.structured_snapshot_from_database(&manifest.id)?,
        };
        snapshot.metrics.retain(|metric| {
            metric
                .labels
                .get(PARSER_LABEL)
                .is_none_or(|parser| !names.contains(parser))
        });
        for output in outputs {
            snapshot.metrics.extend(output.metrics);
        }
        write_structured_snapshot(
            &run_dir,
            &snapshot.metrics,
            &snapshot.fingerprints,
            &snapshot.transitions,
        )?;
        write_manifest(&run_dir, manifest)?;
        Ok(())
    }

    fn structured_snapshot_from_database(&self, id: &str) -> Result<StructuredSnapshot> {
        let metrics = self.metrics(id)?;
        let mut statement = self.connection.prepare(
            "SELECT name, value, labels_json FROM fingerprints WHERE run_id=?1 ORDER BY id",
        )?;
        let fingerprints = statement
            .query_map([id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .map(|row| {
                let (name, value, labels_json) = row?;
                Ok(Fingerprint {
                    name,
                    value,
                    labels: serde_json::from_str(&labels_json)
                        .context("invalid fingerprint labels")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut statement = self.connection.prepare(
            "SELECT from_route, to_route, count, p50_ms, p95_ms FROM transitions WHERE run_id=?1 ORDER BY id",
        )?;
        let transitions = statement
            .query_map([id], |row| {
                Ok(Transition {
                    from_route: row.get(0)?,
                    to_route: row.get(1)?,
                    count: row.get(2)?,
                    p50_ms: row.get(3)?,
                    p95_ms: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(StructuredSnapshot {
            schema_version: STRUCTURED_SNAPSHOT_SCHEMA,
            metrics,
            fingerprints,
            transitions,
        })
    }

    fn restore_missing_finalized_runs(&mut self) -> Result<()> {
        let runs_dir = self.data_dir.join("runs");
        let mut entries = fs::read_dir(&runs_dir)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            if !entry.file_type()?.is_dir() || entry.file_name() == ".incomplete" {
                continue;
            }
            let run_dir = entry.path();
            let manifest_path = run_dir.join("run.json");
            if !manifest_path.is_file() {
                continue;
            }
            let manifest: RunManifest = match fs::read(&manifest_path)
                .with_context(|| format!("cannot read {}", manifest_path.display()))
                .and_then(|raw| {
                    serde_json::from_slice(&raw)
                        .with_context(|| format!("invalid manifest {}", manifest_path.display()))
                }) {
                Ok(manifest) => manifest,
                Err(error) => {
                    eprintln!("! cannot reindex saved run: {error:#}");
                    continue;
                }
            };
            let indexed = self
                .connection
                .query_row("SELECT 1 FROM runs WHERE id=?1", [&manifest.id], |_| Ok(()))
                .optional()?
                .is_some();
            if indexed {
                continue;
            }

            let (metrics, fingerprints, transitions) =
                if let Some(snapshot) = read_structured_snapshot(&run_dir)? {
                    (
                        snapshot.metrics,
                        snapshot.fingerprints,
                        snapshot.transitions,
                    )
                } else {
                    eprintln!(
                        "! {} has no structured snapshot; falling back to protocol logs",
                        manifest.id
                    );
                    structured_records_from_logs(&run_dir, &manifest)?
                };
            self.restore_finalized_run(&manifest, &metrics, &fingerprints, &transitions)?;
            eprintln!("reindexed  {} from saved run", manifest.id);
            if metrics.len() != manifest.metric_count
                || fingerprints.len() != manifest.fingerprint_count
                || transitions.len() != manifest.transition_count
            {
                eprintln!(
                    "! recovered structured rows differ from manifest for {}: metrics {}/{}, fingerprints {}/{}, transitions {}/{}",
                    manifest.id,
                    metrics.len(),
                    manifest.metric_count,
                    fingerprints.len(),
                    manifest.fingerprint_count,
                    transitions.len(),
                    manifest.transition_count,
                );
            }
        }
        Ok(())
    }

    fn restore_finalized_run(
        &mut self,
        manifest: &RunManifest,
        metrics: &[Metric],
        fingerprints: &[Fingerprint],
        transitions: &[Transition],
    ) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO runs (id, started_at, finished_at, mode, state, commit_hash, dirty, score, passed, exit_code, note, hypothesis, analysis_status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                manifest.id,
                manifest.started_at.to_rfc3339(),
                manifest.finished_at.map(|value| value.to_rfc3339()),
                manifest.mode.as_str(),
                manifest.state.as_str(),
                manifest.source.commit_hash,
                manifest.source.dirty,
                manifest.benchmark.score,
                manifest.benchmark.passed,
                manifest.benchmark.exit_code,
                manifest.note,
                manifest.hypothesis,
                manifest.analysis_status.as_str(),
            ],
        )?;
        for tag in &manifest.tags {
            transaction.execute(
                "INSERT INTO run_tags (run_id, tag) VALUES (?1, ?2)",
                params![manifest.id, tag],
            )?;
        }
        for log in &manifest.logs {
            transaction.execute(
                "INSERT INTO logs (id, run_id, kind, node) VALUES (?1, ?2, ?3, ?4)",
                params![log.id, manifest.id, log.kind, log.node],
            )?;
        }
        for metric in metrics {
            transaction.execute(
                "INSERT INTO metrics (run_id, name, value, unit, observed_at, labels_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![manifest.id, metric.name, metric.value, metric.unit, metric.timestamp.map(|value| value.to_rfc3339()), serde_json::to_string(&metric.labels)?],
            )?;
        }
        for fingerprint in fingerprints {
            transaction.execute(
                "INSERT INTO fingerprints (run_id, name, value, labels_json) VALUES (?1, ?2, ?3, ?4)",
                params![manifest.id, fingerprint.name, fingerprint.value, serde_json::to_string(&fingerprint.labels)?],
            )?;
        }
        for transition in transitions {
            transaction.execute(
                "INSERT INTO transitions (run_id, from_route, to_route, count, p50_ms, p95_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![manifest.id, transition.from_route, transition.to_route, transition.count, transition.p50_ms, transition.p95_ms],
            )?;
        }
        for collector in &manifest.collectors {
            transaction.execute(
                "INSERT INTO collector_runs (run_id, name, node, phase, status, exit_code, error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![manifest.id, collector.name, collector.node, collector.phase, collector.status, collector.exit_code, collector.error],
            )?;
        }
        for enrichment in &manifest.enrichments {
            transaction.execute(
                "INSERT INTO enrichment_runs (run_id, name, status, command_json, exit_code, error, log_ids_json, tooling_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    manifest.id,
                    enrichment.name,
                    enrichment.status,
                    serde_json::to_string(&enrichment.command)?,
                    enrichment.exit_code,
                    enrichment.error,
                    serde_json::to_string(&enrichment.log_ids)?,
                    enrichment.tooling_path,
                ],
            )?;
        }
        for analysis in &manifest.analyses {
            transaction.execute(
                "INSERT INTO run_analyses (id, run_id, created_at, verdict, body) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    analysis.id,
                    manifest.id,
                    analysis.created_at.to_rfc3339(),
                    analysis.verdict.as_str(),
                    analysis.body,
                ],
            )?;
        }
        persist_codex_context(&transaction, manifest)?;
        transaction.commit()?;
        Ok(())
    }
}

fn structured_snapshot_path(run_dir: &Path) -> PathBuf {
    run_dir.join("structured.json.zst")
}

fn write_structured_snapshot(
    run_dir: &Path,
    metrics: &[Metric],
    fingerprints: &[Fingerprint],
    transitions: &[Transition],
) -> Result<()> {
    let path = structured_snapshot_path(run_dir);
    let temporary = run_dir.join("structured.json.zst.tmp");
    let output = fs::File::create(&temporary)?;
    let mut encoder = zstd::stream::write::Encoder::new(output, 3)?;
    serde_json::to_writer(
        &mut encoder,
        &StructuredSnapshot {
            schema_version: STRUCTURED_SNAPSHOT_SCHEMA,
            metrics: metrics.to_vec(),
            fingerprints: fingerprints.to_vec(),
            transitions: transitions.to_vec(),
        },
    )?;
    encoder.finish()?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn read_structured_snapshot(run_dir: &Path) -> Result<Option<StructuredSnapshot>> {
    let path = structured_snapshot_path(run_dir);
    if !path.is_file() {
        return Ok(None);
    }
    let input = fs::File::open(&path)?;
    let decoder = zstd::stream::read::Decoder::new(input)?;
    let snapshot: StructuredSnapshot = serde_json::from_reader(decoder)
        .with_context(|| format!("invalid structured snapshot {}", path.display()))?;
    if snapshot.schema_version != STRUCTURED_SNAPSHOT_SCHEMA {
        bail!(
            "unsupported structured snapshot schema {} in {}",
            snapshot.schema_version,
            path.display()
        );
    }
    Ok(Some(snapshot))
}

fn structured_records_from_logs(
    run_dir: &Path,
    manifest: &RunManifest,
) -> Result<(Vec<Metric>, Vec<Fingerprint>, Vec<Transition>)> {
    let mut metrics = Vec::new();
    let mut fingerprints = Vec::new();
    let mut transitions = Vec::new();
    for log in &manifest.logs {
        let parser_name = log
            .kind
            .strip_prefix("benchmark-parser:")
            .and_then(|rest| rest.strip_suffix(":stdout"));
        let inline_benchmark =
            log.kind == "benchmark-stdout" && manifest.mode != crate::model::RunMode::ScoreRun;
        if (!log.kind.starts_with("collector:") || !log.kind.ends_with(":stdout"))
            && parser_name.is_none()
            && !inline_benchmark
        {
            continue;
        }
        let path = run_dir.join("logs").join(format!("{}.zst", log.id));
        let (mut parsed_metrics, mut parsed_fingerprints, mut parsed_transitions) =
            match collector::parse_protocol(&path) {
                Ok(records) => records,
                Err(error) => {
                    eprintln!(
                        "! cannot recover structured data from {}: {error:#}",
                        path.display()
                    );
                    continue;
                }
            };
        if let Some(node) = &log.node {
            for metric in &mut parsed_metrics {
                metric
                    .labels
                    .entry("node".into())
                    .or_insert_with(|| node.clone());
            }
            for fingerprint in &mut parsed_fingerprints {
                fingerprint
                    .labels
                    .entry("node".into())
                    .or_insert_with(|| node.clone());
            }
        }
        if let Some(collector_name) = log
            .kind
            .strip_prefix("collector:")
            .and_then(|kind| kind.strip_suffix(":stdout"))
        {
            for metric in &mut parsed_metrics {
                metric
                    .labels
                    .entry("collector".into())
                    .or_insert_with(|| collector_name.to_owned());
            }
        }
        if let Some(parser_name) = parser_name {
            for metric in &mut parsed_metrics {
                metric
                    .labels
                    .insert(PARSER_LABEL.into(), parser_name.to_owned());
            }
            parsed_fingerprints.clear();
            parsed_transitions.clear();
        } else if inline_benchmark {
            for metric in &mut parsed_metrics {
                metric.labels.insert(PARSER_LABEL.into(), "inline".into());
            }
            parsed_fingerprints.clear();
            parsed_transitions.clear();
        }
        metrics.extend(parsed_metrics);
        fingerprints.extend(parsed_fingerprints);
        transitions.extend(parsed_transitions);
    }
    if manifest.mode != crate::model::RunMode::ScoreRun
        && let (Some(start), Some(end)) = (
            manifest.benchmark.initialize_started_at,
            manifest.benchmark.initialize_finished_at,
        )
    {
        metrics.push(Metric {
            name: "benchmark.initialize_duration".into(),
            value: (end - start).num_microseconds().unwrap_or_default() as f64 / 1_000.0,
            unit: "ms".into(),
            timestamp: None,
            labels: Default::default(),
        });
    }
    Ok((metrics, fingerprints, transitions))
}

pub(crate) fn write_manifest(run_dir: &Path, manifest: &RunManifest) -> Result<()> {
    let path = run_dir.join("run.json");
    let temporary = run_dir.join("run.json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(manifest)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn migrate(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS runs (
            id TEXT PRIMARY KEY,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            mode TEXT NOT NULL,
            state TEXT NOT NULL,
            commit_hash TEXT,
            dirty INTEGER NOT NULL,
            score INTEGER,
            passed INTEGER,
            exit_code INTEGER,
            note TEXT,
            hypothesis TEXT NOT NULL DEFAULT '',
            analysis_status TEXT NOT NULL DEFAULT 'not_required'
        );
        CREATE TABLE IF NOT EXISTS logs (
            id TEXT NOT NULL,
            run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            node TEXT,
            PRIMARY KEY (run_id, id)
        );
        CREATE TABLE IF NOT EXISTS metrics (
            id INTEGER PRIMARY KEY,
            run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            value REAL NOT NULL,
            unit TEXT NOT NULL,
            observed_at TEXT,
            labels_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS metrics_run_name ON metrics(run_id, name);
        CREATE TABLE IF NOT EXISTS fingerprints (
            id INTEGER PRIMARY KEY,
            run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            value TEXT NOT NULL,
            labels_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS fingerprints_run_name ON fingerprints(run_id, name);
        CREATE TABLE IF NOT EXISTS transitions (
            id INTEGER PRIMARY KEY,
            run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            from_route TEXT NOT NULL,
            to_route TEXT NOT NULL,
            count INTEGER NOT NULL,
            p50_ms REAL,
            p95_ms REAL
        );
        CREATE TABLE IF NOT EXISTS collector_runs (
            id INTEGER PRIMARY KEY,
            run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            node TEXT,
            phase TEXT NOT NULL,
            status TEXT NOT NULL,
            exit_code INTEGER,
            error TEXT
        );
        CREATE TABLE IF NOT EXISTS run_tags (
            run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            tag TEXT NOT NULL,
            PRIMARY KEY (run_id, tag)
        );
        CREATE INDEX IF NOT EXISTS run_tags_tag ON run_tags(tag);
        CREATE TABLE IF NOT EXISTS enrichment_runs (
            run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            status TEXT NOT NULL,
            command_json TEXT NOT NULL,
            exit_code INTEGER,
            error TEXT,
            log_ids_json TEXT NOT NULL,
            tooling_path TEXT,
            PRIMARY KEY (run_id, name)
        );
        CREATE TABLE IF NOT EXISTS run_analyses (
            id TEXT PRIMARY KEY,
            run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            created_at TEXT NOT NULL,
            verdict TEXT NOT NULL,
            body TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS run_analyses_run_created ON run_analyses(run_id, created_at);
        CREATE TABLE IF NOT EXISTS run_codex_context (
            run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
            history_path TEXT NOT NULL,
            session_id TEXT NOT NULL,
            input_id TEXT NOT NULL,
            snapshot_path TEXT NOT NULL,
            sha256 TEXT NOT NULL
        );
        ",
    )?;
    ensure_column(connection, "runs", "note", "TEXT")?;
    ensure_column(connection, "runs", "hypothesis", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(
        connection,
        "runs",
        "analysis_status",
        "TEXT NOT NULL DEFAULT 'not_required'",
    )?;
    ensure_column(connection, "metrics", "observed_at", "TEXT")?;
    connection.pragma_update(None, "user_version", 6)?;
    Ok(())
}

fn persist_codex_context(connection: &Connection, manifest: &RunManifest) -> Result<()> {
    let Some(context) = &manifest.codex_context else {
        return Ok(());
    };
    connection.execute(
        "INSERT OR REPLACE INTO run_codex_context (run_id, history_path, session_id, input_id, snapshot_path, sha256) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            manifest.id,
            context.history_path,
            context.session_id,
            context.input_id,
            context.snapshot_path,
            context.sha256,
        ],
    )?;
    Ok(())
}

fn ensure_column(connection: &Connection, table: &str, column: &str, kind: &str) -> Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|value| value == column) {
        connection.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BenchmarkResult, RunMode, SourceSnapshot, ToolingSnapshot};
    use tempfile::tempdir;

    #[test]
    fn recovers_incomplete_run_as_aborted() {
        let directory = tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let manifest = RunManifest {
            schema_version: 6,
            id: "01a03df2-ecb2-72b3-aa1f-c952d3dd102b".into(),
            mode: RunMode::Run,
            state: RunState::Running,
            started_at: Utc::now(),
            finished_at: None,
            hypothesis: "test recovery".into(),
            analysis_status: AnalysisStatus::Pending,
            analyses: Vec::new(),
            note: None,
            tags: Vec::new(),
            source: SourceSnapshot::default(),
            tooling: ToolingSnapshot::default(),
            codex_context: None,
            benchmark: BenchmarkResult::default(),
            collectors: Vec::new(),
            enrichments: Vec::new(),
            logs: Vec::new(),
            metric_count: 0,
            fingerprint_count: 0,
            transition_count: 0,
        };
        store.begin(&manifest).unwrap();
        drop(store);

        let mut store = Store::open(directory.path()).unwrap();
        assert_eq!(
            store.recover_incomplete().unwrap(),
            vec![manifest.id.clone()]
        );
        let recovered = store.load(&manifest.id).unwrap();
        assert!(matches!(recovered.state, RunState::Aborted));
        assert!(recovered.benchmark.interrupted);
        assert!(!store.staging_dir(&manifest.id).exists());
        assert!(store.final_dir(&manifest.id).is_dir());
    }
}
