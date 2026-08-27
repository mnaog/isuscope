use crate::{
    collector,
    model::{Fingerprint, Metric, RunManifest, RunState, Transition},
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct Store {
    data_dir: PathBuf,
    connection: Connection,
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
            write_manifest(&staging, &manifest)?;
            let final_dir = self.final_dir(&manifest.id);
            if final_dir.exists() {
                bail!("cannot recover {}; final run already exists", manifest.id);
            }
            fs::rename(&staging, &final_dir)?;
            self.connection.execute(
                "INSERT OR IGNORE INTO runs (id, started_at, mode, state, commit_hash, dirty) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    manifest.id,
                    manifest.started_at.to_rfc3339(),
                    manifest.mode.as_str(),
                    manifest.state.as_str(),
                    manifest.source.commit_hash,
                    manifest.source.dirty,
                ],
            )?;
            self.connection.execute(
                "UPDATE runs SET finished_at=?2, state=?3, score=?4, passed=0, exit_code=?5 WHERE id=?1",
                params![
                    manifest.id,
                    manifest.finished_at.map(|value| value.to_rfc3339()),
                    manifest.state.as_str(),
                    manifest.benchmark.score,
                    manifest.benchmark.exit_code,
                ],
            )?;
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
            "INSERT INTO runs (id, started_at, mode, state, commit_hash, dirty) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                manifest.id,
                manifest.started_at.to_rfc3339(),
                manifest.mode.as_str(),
                manifest.state.as_str(),
                manifest.source.commit_hash,
                manifest.source.dirty,
            ],
        )?;
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
        let final_dir = self.final_dir(&manifest.id);
        fs::rename(&staging, &final_dir)
            .with_context(|| format!("cannot finalize run directory {}", final_dir.display()))?;

        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE runs SET finished_at=?2, state=?3, commit_hash=?4, dirty=?5, score=?6, passed=?7, exit_code=?8 WHERE id=?1",
            params![
                manifest.id,
                manifest.finished_at.map(|value| value.to_rfc3339()),
                manifest.state.as_str(),
                manifest.source.commit_hash,
                manifest.source.dirty,
                manifest.benchmark.score,
                manifest.benchmark.passed,
                manifest.benchmark.exit_code,
            ],
        )?;
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
        transaction.commit()?;
        Ok(final_dir)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<RunSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT id, started_at, commit_hash, dirty, mode, state, score, passed FROM runs ORDER BY started_at DESC LIMIT ?1",
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
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
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
                structured_records_from_logs(&run_dir, &manifest)?;
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
            "INSERT INTO runs (id, started_at, finished_at, mode, state, commit_hash, dirty, score, passed, exit_code) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
            ],
        )?;
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
        transaction.commit()?;
        Ok(())
    }
}

fn structured_records_from_logs(
    run_dir: &Path,
    manifest: &RunManifest,
) -> Result<(Vec<Metric>, Vec<Fingerprint>, Vec<Transition>)> {
    let mut metrics = Vec::new();
    let mut fingerprints = Vec::new();
    let mut transitions = Vec::new();
    for log in &manifest.logs {
        if !log.kind.starts_with("collector:") || !log.kind.ends_with(":stdout") {
            continue;
        }
        let path = run_dir.join("logs").join(format!("{}.zst", log.id));
        let (mut parsed_metrics, mut parsed_fingerprints, parsed_transitions) =
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
        metrics.extend(parsed_metrics);
        fingerprints.extend(parsed_fingerprints);
        transitions.extend(parsed_transitions);
    }
    if let (Some(start), Some(end)) = (
        manifest.benchmark.initialize_started_at,
        manifest.benchmark.initialize_finished_at,
    ) {
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

fn write_manifest(run_dir: &Path, manifest: &RunManifest) -> Result<()> {
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
            exit_code INTEGER
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
        PRAGMA user_version = 3;
        ",
    )?;
    let has_observed_at = connection
        .prepare("PRAGMA table_info(metrics)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|column| column == "observed_at");
    if !has_observed_at {
        connection.execute("ALTER TABLE metrics ADD COLUMN observed_at TEXT", [])?;
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
            schema_version: 4,
            id: "01a03df2-ecb2-72b3-aa1f-c952d3dd102b".into(),
            mode: RunMode::Run,
            state: RunState::Running,
            started_at: Utc::now(),
            finished_at: None,
            source: SourceSnapshot::default(),
            tooling: ToolingSnapshot::default(),
            benchmark: BenchmarkResult::default(),
            collectors: Vec::new(),
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
