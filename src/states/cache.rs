//! Disposable SQLite beside the raw logs. Collector workers append their already parsed
//! events under a per-run lock; readers only reuse a cache for exactly the requested inputs.
use super::*;
use rusqlite::OpenFlags;
use serde::{Deserialize, Serialize};
use std::{fs, time::UNIX_EPOCH};

// Bump when parsing, event ordering, tables, or derived columns change.
pub(super) const VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SourceStamp {
    file: String,
    node: String,
    bytes: u64,
    modified: (u64, u32),
    // Also notice replacements and same-size writes with a preserved mtime.
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl SourceStamp {
    pub(super) fn of(path: &Path, node: &str) -> Result<Self> {
        let metadata = fs::metadata(path)?;
        let modified = metadata.modified()?.duration_since(UNIX_EPOCH)?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            file: path
                .file_name()
                .context("state log has no filename")?
                .to_str()
                .context("state log filename is not UTF-8")?
                .into(),
            node: node.into(),
            bytes: metadata.len(),
            modified: (modified.as_secs(), modified.subsec_nanos()),
            #[cfg(unix)]
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ),
        })
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ContextKey {
    version: u32,
    window: Option<(String, i64, i64)>,
    started: Option<i64>,
}

impl ContextKey {
    fn new(window: Option<Window>, started: Option<DateTime<Utc>>) -> Self {
        Self {
            version: VERSION,
            window: window.map(|w| (w.name.into(), w.start, w.end)),
            started: started.map(|t| t.timestamp_micros()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    stamp: SourceStamp,
    events: usize,
    invalid_lines: u64,
    dropped: u64,
}

impl Entry {
    fn new(stamp: SourceStamp, parsed: &Parsed) -> Self {
        Self {
            stamp,
            events: parsed.events.len(),
            invalid_lines: parsed.invalid_lines,
            dropped: parsed.dropped,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Metadata {
    context: ContextKey,
    sources: Vec<Entry>,
}

fn directory(logs_dir: &Path) -> PathBuf {
    // logs/ is already excluded from Git. Moving the run out of .incomplete keeps the
    // cache with its evidence, and does not change individual log-file stamps.
    logs_dir.join(".state-cache")
}

fn lock(directory: &Path) -> Result<fs::File> {
    fs::create_dir_all(directory)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("lock"))?;
    file.lock()?;
    Ok(file) // released by the OS on drop, including process exit
}

fn metadata(connection: &Connection) -> Result<Metadata> {
    let value: String = connection.query_row(
        "SELECT value FROM state_cache_metadata WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&value)?)
}

fn save_metadata(connection: &Connection, metadata: &Metadata) -> Result<()> {
    connection.execute(
        "INSERT OR REPLACE INTO state_cache_metadata (id, value) VALUES (1, ?1)",
        [serde_json::to_string(metadata)?],
    )?;
    Ok(())
}

fn loaded(connection: Connection, metadata: Metadata) -> Result<Loaded> {
    // Detect missing/incompatible tables as a cache miss, even if metadata survived.
    connection.prepare("SELECT * FROM state_events LIMIT 0")?;
    connection.prepare("SELECT * FROM state_stays LIMIT 0")?;
    connection.prepare("SELECT * FROM state_attrs LIMIT 0")?;
    connection.prepare("SELECT * FROM state_window LIMIT 0")?;
    connection.pragma_update(None, "query_only", true)?;
    Ok(Loaded {
        events: metadata.sources.iter().map(|s| s.events).sum(),
        invalid_lines: metadata.sources.iter().map(|s| s.invalid_lines).sum(),
        dropped: metadata.sources.iter().map(|s| s.dropped).sum(),
        connection,
    })
}

fn cached(path: &Path, context: &ContextKey, stamps: &[SourceStamp]) -> Result<Loaded> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let metadata = metadata(&connection)?;
    anyhow::ensure!(metadata.context == *context, "state cache context changed");
    // Keep source order too: ties in (t, seq) must retain the original insertion order.
    anyhow::ensure!(
        metadata.sources.iter().map(|s| &s.stamp).eq(stamps.iter()),
        "state cache inputs changed"
    );
    loaded(connection, metadata)
}

struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(format!("{}-journal", self.0.display()));
    }
}

/// Publish only a complete database. A failed rebuild leaves any previous cache untouched.
fn build(
    destination: &Path,
    window: Option<Window>,
    started: Option<DateTime<Utc>>,
    fill: impl FnOnce(&Connection) -> Result<Vec<Entry>>,
) -> Result<()> {
    let temporary =
        Temporary(destination.with_file_name(format!("{}.sqlite3", uuid::Uuid::now_v7())));
    let mut connection = Connection::open(&temporary.0)?;
    let transaction = connection.transaction()?;
    transaction.execute_batch(state_tables!())?;
    transaction.execute_batch(
        "CREATE TABLE state_cache_metadata (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL);"
    )?;
    let sources = fill(&transaction)?;
    insert_window(&transaction, window)?;
    transaction.execute_batch(concat!(state_stays!(), state_indexes!()))?;
    save_metadata(
        &transaction,
        &Metadata {
            context: ContextKey::new(window, started),
            sources,
        },
    )?;
    transaction.commit()?;
    connection.close().map_err(|(_, error)| error)?;
    fs::rename(&temporary.0, destination)?;
    Ok(())
}

pub(super) fn warm(
    path: &Path,
    stamp: SourceStamp,
    parsed: &Parsed,
    window: Option<Window>,
    started: Option<DateTime<Utc>>,
) -> Result<()> {
    let dir = directory(path.parent().context("state log has no parent directory")?);
    let _lock = lock(&dir)?;
    anyhow::ensure!(
        SourceStamp::of(path, &stamp.node)? == stamp,
        "state log changed during parsing"
    );
    let destination = dir.join("states.sqlite3");
    let context = ContextKey::new(window, started);
    let previous = Connection::open_with_flags(&destination, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .ok()
        .and_then(|connection| {
            metadata(&connection)
                .ok()
                .filter(|m| m.context == context)
                .map(|m| (connection, m))
        });
    if let Some((mut connection, mut metadata)) = previous {
        if let Some(entry) = metadata.sources.iter().find(|e| e.stamp.file == stamp.file) {
            anyhow::ensure!(
                entry.stamp == stamp,
                "state log was already cached with different inputs"
            );
            return Ok(());
        }
        let transaction = connection.transaction()?;
        insert_events(&transaction, &stamp.node, parsed, started)?;
        transaction.execute_batch("DROP TABLE state_stays")?;
        transaction.execute_batch(state_stays!())?;
        transaction.execute_batch(
            "CREATE INDEX state_stays_state ON state_stays (kind, state, entered_t);
             CREATE INDEX state_stays_object ON state_stays (kind, id, entered_t);",
        )?;
        metadata.sources.push(Entry::new(stamp, parsed));
        save_metadata(&transaction, &metadata)?;
        transaction.commit()?;
    } else {
        build(&destination, window, started, |connection| {
            insert_events(connection, &stamp.node, parsed, started)?;
            Ok(vec![Entry::new(stamp, parsed)])
        })?;
    }
    Ok(())
}

/// Collectors finish in arbitrary order. Restore manifest order once, before publishing
/// the run, using the cached rows rather than decoding and parsing the raw logs again.
pub fn prepare_cache_order(
    run_dir: &Path,
    sources: &[Source],
    window: Option<Window>,
    started: Option<DateTime<Utc>>,
) -> Result<()> {
    if sources.len() < 2 {
        return Ok(());
    }
    let dir = directory(&run_dir.join("logs"));
    let destination = dir.join("states.sqlite3");
    if !destination.is_file() {
        return Ok(());
    }
    let stamps = sources
        .iter()
        .map(|s| SourceStamp::of(&s.path, &s.node))
        .collect::<Result<Vec<_>>>()?;
    let _lock = lock(&dir)?;
    reorder(&destination, &stamps, window, started)?;
    Ok(())
}

fn reorder(
    destination: &Path,
    stamps: &[SourceStamp],
    window: Option<Window>,
    started: Option<DateTime<Utc>>,
) -> Result<bool> {
    let connection = Connection::open_with_flags(destination, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let metadata = metadata(&connection)?;
    drop(connection);
    if metadata.context != ContextKey::new(window, started)
        || metadata.sources.len() != stamps.len()
    {
        return Ok(false);
    }
    if metadata.sources.iter().map(|s| &s.stamp).eq(stamps.iter()) {
        return Ok(false);
    }
    let mut next_row = 1_i64;
    let ranges = metadata
        .sources
        .iter()
        .map(|entry| {
            let first = next_row;
            next_row += entry.events as i64;
            (entry, first, next_row - 1)
        })
        .collect::<Vec<_>>();
    let ordered = stamps
        .iter()
        .map(|stamp| {
            ranges
                .iter()
                .find(|(entry, _, _)| entry.stamp == *stamp)
                .copied()
        })
        .collect::<Option<Vec<_>>>();
    let Some(ordered) = ordered else {
        return Ok(false);
    };
    build(destination, window, started, |connection| {
        connection.execute(
            "ATTACH DATABASE ?1 AS state_input",
            [destination.to_str().context("non-UTF8 cache path")?],
        )?;
        for (_, first, last) in &ordered {
            connection.execute(
                "INSERT INTO state_events SELECT * FROM state_input.state_events
                 WHERE rowid BETWEEN ?1 AND ?2 ORDER BY rowid",
                rusqlite::params![first, last],
            )?;
        }
        connection.execute(
            "INSERT INTO state_attrs SELECT * FROM state_input.state_attrs ORDER BY rowid",
            [],
        )?;
        Ok(ordered
            .iter()
            .map(|(entry, _, _)| (*entry).clone())
            .collect())
    })?;
    Ok(true)
}

/// Seed the bounded query cache from collector output without decoding the logs again.
/// Hold the writer lock while copying, so concurrent collectors cannot produce a torn file.
pub(super) fn copy_prepared(
    destination: &Path,
    sources: &[Source],
    window: Option<Window>,
    started: Option<DateTime<Utc>>,
) -> Result<Option<(usize, u64, u64)>> {
    let Some(logs_dir) = sources.first().and_then(|source| source.path.parent()) else {
        return Ok(None);
    };
    if sources
        .iter()
        .any(|source| source.path.parent() != Some(logs_dir))
    {
        return Ok(None);
    }
    let dir = directory(logs_dir);
    let prepared = dir.join("states.sqlite3");
    if !prepared.is_file() {
        return Ok(None);
    }
    let stamps = sources
        .iter()
        .map(|source| SourceStamp::of(&source.path, &source.node))
        .collect::<Result<Vec<_>>>()?;
    let _lock = lock(&dir)?;
    let loaded = match cached(&prepared, &ContextKey::new(window, started), &stamps) {
        Ok(loaded) => loaded,
        Err(_) => return Ok(None),
    };
    if let Err(error) = fs::copy(&prepared, destination) {
        let _ = fs::remove_file(destination);
        return Err(error.into());
    }
    Ok(Some((loaded.events, loaded.invalid_lines, loaded.dropped)))
}

/// Read a prepared cache, or rebuild one for older runs and changed/missing caches.
/// Cache storage failures fall back to the original in-memory path.
#[cfg(test)]
fn load_cached(
    run_dir: &Path,
    sources: &[Source],
    window: Option<Window>,
    started: Option<DateTime<Utc>>,
) -> Result<Loaded> {
    if sources.is_empty() {
        return load(sources, window, started);
    }
    match open_or_build(run_dir, sources, window, started) {
        Ok(loaded) => Ok(loaded),
        Err(error) => {
            eprintln!("! state cache unavailable; reading original logs: {error:#}");
            load(sources, window, started)
        }
    }
}

#[cfg(test)]
fn open_or_build(
    run_dir: &Path,
    sources: &[Source],
    window: Option<Window>,
    started: Option<DateTime<Utc>>,
) -> Result<Loaded> {
    let stamps = sources
        .iter()
        .map(|s| SourceStamp::of(&s.path, &s.node))
        .collect::<Result<Vec<_>>>()?;
    let context = ContextKey::new(window, started);
    let dir = directory(&run_dir.join("logs"));
    let destination = dir.join("states.sqlite3");
    if let Ok(loaded) = cached(&destination, &context, &stamps) {
        return Ok(loaded);
    }
    let _lock = lock(&dir)?;
    if let Ok(loaded) = cached(&destination, &context, &stamps) {
        return Ok(loaded);
    }
    if reorder(&destination, &stamps, window, started).unwrap_or(false) {
        return cached(&destination, &context, &stamps);
    }
    build(&destination, window, started, |connection| {
        sources
            .iter()
            .zip(&stamps)
            .map(|(source, stamp)| {
                let parsed = read(std::io::BufReader::new(zstd::stream::read::Decoder::new(
                    fs::File::open(&source.path)?,
                )?));
                anyhow::ensure!(
                    SourceStamp::of(&source.path, &source.node)? == *stamp,
                    "state log changed during cache rebuild"
                );
                insert_events(connection, &source.node, &parsed, started)?;
                Ok(Entry::new(stamp.clone(), &parsed))
            })
            .collect()
    })?;
    cached(&destination, &context, &stamps)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &[u8] =
        br#"{"t":2000000,"seq":2,"kind":"item","id":"x","from":"A","to":"B","attrs":{"peer":"y"}}
{"t":1000000,"seq":1,"kind":"item","id":"x","from":null,"to":"A"}
{"t":2000100,"seq":3,"kind":"peer","id":"y","to":"READY","attrs":{"item":"x"}}
{"t":2000200,"dropped":3}
bad line
"#;

    fn source(run: &Path, name: &str, node: &str, raw: &[u8]) -> Source {
        fs::create_dir_all(run.join("logs")).unwrap();
        let path = run.join("logs").join(name);
        fs::write(&path, zstd::encode_all(raw, 1).unwrap()).unwrap();
        Source {
            node: node.into(),
            path,
        }
    }

    fn database(run: &Path) -> PathBuf {
        directory(&run.join("logs")).join("states.sqlite3")
    }

    fn rows(loaded: &Loaded, sql: &str) -> Value {
        serde_json::to_value(crate::sql::run(&loaded.connection, sql, [], 100).unwrap()).unwrap()
    }

    fn equivalent(actual: &Loaded, expected: &Loaded) {
        assert_eq!(
            (actual.events, actual.invalid_lines, actual.dropped),
            (expected.events, expected.invalid_lines, expected.dropped)
        );
        for sql in [
            "SELECT * FROM state_events ORDER BY rowid",
            "SELECT * FROM state_stays ORDER BY kind,id,entered_t",
            "SELECT * FROM state_window",
            "SELECT * FROM state_attrs ORDER BY node,t,seq,kind,id,key,value",
        ] {
            assert_eq!(rows(actual, sql), rows(expected, sql), "{sql}");
        }
        let timeline = |loaded: &Loaded| {
            serde_json::to_value(crate::sql::run(
                &loaded.connection, TIMELINE,
                rusqlite::named_params! {":kind":"item", ":id":"x", ":around":Option::<i64>::None, ":span":1000, ":related":true},
                100,
            ).unwrap()).unwrap()
        };
        assert_eq!(timeline(actual), timeline(expected));
        assert!(
            actual
                .connection
                .execute("DELETE FROM state_events", [])
                .is_err()
        );
    }

    #[test]
    fn collection_prepares_first_query_without_changing_metrics_or_warnings() {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path().join("incomplete");
        let src = source(&run, "app.zst", "app", RAW);
        let window = Some(Window {
            name: "load",
            start: 1_000_000,
            end: 3_000_000,
        });
        let started = DateTime::from_timestamp_micros(500_000);
        let expected = parse_collector_output(&src.path, window).unwrap();
        let actual = collect(&src.path, &src.node, window, started).unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        let before = fs::metadata(database(&run)).unwrap().modified().unwrap();
        // The runner moves the whole directory after collection.
        let final_run = temp.path().join("finished");
        fs::rename(&run, &final_run).unwrap();
        let sources = [Source {
            node: src.node,
            path: final_run.join("logs/app.zst"),
        }];
        let expected = load(&sources, window, started).unwrap();
        let actual = load_cached(&final_run, &sources, window, started).unwrap();
        assert_eq!(
            Path::new(actual.connection.path().unwrap())
                .canonicalize()
                .unwrap(),
            database(&final_run).canonicalize().unwrap()
        );
        equivalent(&actual, &expected);
        assert_eq!(actual.invalid_lines, 1);
        assert_eq!(actual.dropped, 3);
        drop(actual);
        drop(load_cached(&final_run, &sources, window, started).unwrap());
        assert_eq!(
            before,
            fs::metadata(database(&final_run))
                .unwrap()
                .modified()
                .unwrap()
        );
    }

    #[test]
    fn query_cache_copies_prepared_tables_without_rebuilding_them() {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path().join("run");
        let sources = [source(&run, "app.zst", "app", RAW)];
        collect(&sources[0].path, "app", None, None).unwrap();
        let modified = fs::metadata(database(&run)).unwrap().modified().unwrap();
        let cache = temp.path().join("queries");
        let actual = super::super::load_cached(&cache, "run", &sources, None, None).unwrap();
        equivalent(&actual, &load(&sources, None, None).unwrap());
        // This table is only copied from the prepared cache, never made by a raw-log rebuild.
        assert_eq!(
            actual
                .connection
                .query_row("SELECT COUNT(*) FROM state_cache_metadata", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );
        assert_eq!(
            fs::metadata(database(&run)).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(
            Path::new(actual.connection.path().unwrap())
                .canonicalize()
                .unwrap(),
            cache.join("run.sqlite3").canonicalize().unwrap()
        );
    }

    #[test]
    fn changed_inputs_context_version_and_broken_or_deleted_cache_rebuild() {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path();
        let mut sources = vec![source(run, "app.zst", "app", RAW)];
        let check = |sources: &[Source], window, started| {
            equivalent(
                &load_cached(run, sources, window, started).unwrap(),
                &load(sources, window, started).unwrap(),
            );
        };
        check(&sources, None, None);
        // Rewrite with exactly the same compressed length and preserve mtime.
        let modified = fs::metadata(&sources[0].path).unwrap().modified().unwrap();
        let raw = String::from_utf8(RAW.to_vec())
            .unwrap()
            .replace("\"B\"", "\"C\"");
        let replacement = zstd::encode_all(raw.as_bytes(), 1).unwrap();
        assert_eq!(
            replacement.len() as u64,
            fs::metadata(&sources[0].path).unwrap().len()
        );
        fs::write(&sources[0].path, replacement).unwrap();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&sources[0].path)
            .unwrap();
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        // ctime is available on Unix; elsewhere changing mtime is the portable signal.
        #[cfg(not(unix))]
        file.set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
            .unwrap();
        check(&sources, None, None);
        sources[0].node = "renamed".into();
        check(&sources, None, None);
        let window = Some(Window {
            name: "load",
            start: 1_500_000,
            end: 2_500_000,
        });
        let started = DateTime::from_timestamp_micros(1_000_000);
        check(&sources, window, started);
        let connection = Connection::open(database(run)).unwrap();
        let mut meta = metadata(&connection).unwrap();
        meta.context.version += 1;
        save_metadata(&connection, &meta).unwrap();
        drop(connection);
        check(&sources, window, started);
        fs::write(database(run), b"not a database").unwrap();
        check(&sources, window, started);
        fs::remove_file(database(run)).unwrap();
        check(&sources, window, started);
        sources.push(source(run, "other.zst", "other", RAW));
        check(&sources, window, started);
        sources.remove(0);
        check(&sources, window, started);
        fs::remove_file(&sources[0].path).unwrap();
        check(&[], window, started); // removed raw logs must not be resurrected from cache
    }

    #[test]
    fn concurrent_collectors_and_queries_preserve_all_nodes_and_source_order() {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path();
        let sources = (0..4)
            .map(|i| source(run, &format!("{i}.zst"), &format!("node{i}"), RAW))
            .collect::<Vec<_>>();
        let barrier = std::sync::Barrier::new(sources.len());
        std::thread::scope(|scope| {
            for src in &sources {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    collect(&src.path, &src.node, None, None).unwrap();
                });
            }
        });
        let connection = Connection::open(database(run)).unwrap();
        assert_eq!(metadata(&connection).unwrap().sources.len(), 4);
        drop(connection);
        let expected = load(&sources, None, None).unwrap();
        equivalent(&load_cached(run, &sources, None, None).unwrap(), &expected);
        // Force simultaneous first queries of a legacy run.
        fs::remove_file(database(run)).unwrap();
        std::thread::scope(|scope| {
            let handles = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        rows(
                            &load_cached(run, &sources, None, None).unwrap(),
                            "SELECT * FROM state_events ORDER BY rowid",
                        )
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                assert_eq!(
                    handle.join().unwrap(),
                    rows(&expected, "SELECT * FROM state_events ORDER BY rowid")
                );
            }
        });
        let reversed = sources.into_iter().rev().collect::<Vec<_>>();
        equivalent(
            &load_cached(run, &reversed, None, None).unwrap(),
            &load(&reversed, None, None).unwrap(),
        );
    }

    #[test]
    fn finalization_orders_cached_sources_before_the_first_query() {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path();
        let sources = [
            source(run, "first.zst", "first", RAW),
            source(run, "empty.zst", "empty", b"{\"t\":1,\"dropped\":5}\n"),
            source(run, "last.zst", "last", RAW),
        ];
        // Finish in reverse order, including one log with no events.
        for src in sources.iter().rev() {
            collect(&src.path, &src.node, None, None).unwrap();
        }
        prepare_cache_order(run, &sources, None, None).unwrap();
        let stamp = fs::metadata(database(run)).unwrap().modified().unwrap();
        equivalent(
            &load_cached(run, &sources, None, None).unwrap(),
            &load(&sources, None, None).unwrap(),
        );
        assert_eq!(
            stamp,
            fs::metadata(database(run)).unwrap().modified().unwrap()
        );
        // Corruption with intact metadata must rebuild too, not keep falling back.
        Connection::open(database(run))
            .unwrap()
            .execute_batch("DROP TABLE state_stays")
            .unwrap();
        let restored = load_cached(run, &sources, None, None).unwrap();
        assert!(restored.connection.path().is_some());
        equivalent(&restored, &load(&sources, None, None).unwrap());
    }

    #[test]
    fn cache_failure_does_not_fail_collection_or_queries() {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path();
        let sources = [source(run, "app.zst", "app", RAW)];
        fs::write(
            directory(&run.join("logs")),
            b"cannot create a directory here",
        )
        .unwrap();
        let expected = parse_collector_output(&sources[0].path, None).unwrap();
        let actual = collect(&sources[0].path, "app", None, None).unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        equivalent(
            &load_cached(run, &sources, None, None).unwrap(),
            &load(&sources, None, None).unwrap(),
        );
    }

    #[test]
    fn failed_rebuild_does_not_publish_a_partial_database() {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path();
        let sources = [source(run, "app.zst", "app", RAW)];
        drop(load_cached(run, &sources, None, None).unwrap());
        let destination = database(run);
        let before = fs::read(&destination).unwrap();
        assert!(
            build(&destination, None, None, |connection| {
                insert_events(connection, "app", &read(RAW), None)?;
                anyhow::bail!("simulated failure before publication")
            })
            .is_err()
        );
        assert_eq!(before, fs::read(&destination).unwrap());
        let files = fs::read_dir(destination.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            files,
            BTreeSet::from(["lock".into(), "states.sqlite3".into()])
        );
    }

    #[test]
    #[ignore = "local benchmark; set ISUSCOPE_STATE_BENCH_RUN to a saved run directory"]
    fn benchmark_saved_run_cache() {
        use std::time::Instant;
        let run = PathBuf::from(
            std::env::var_os("ISUSCOPE_STATE_BENCH_RUN").expect("set ISUSCOPE_STATE_BENCH_RUN"),
        );
        let manifest: crate::model::RunManifest =
            serde_json::from_slice(&fs::read(run.join("run.json")).unwrap()).unwrap();
        let original = sources(
            &manifest,
            &run.join("logs"),
            &BTreeSet::from([DELTA_COLLECTOR.into()]),
        );
        assert!(!original.is_empty());
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("logs")).unwrap();
        let sources = original
            .into_iter()
            .map(|s| {
                let path = temp.path().join("logs").join(s.path.file_name().unwrap());
                fs::copy(s.path, &path).unwrap();
                Source { node: s.node, path }
            })
            .collect::<Vec<_>>();
        let window = Window::of(&manifest.benchmark);
        let started = manifest.benchmark.started_at;
        let start = Instant::now();
        let before = sources
            .iter()
            .map(|s| parse_collector_output(&s.path, window).unwrap())
            .collect::<Vec<_>>();
        let metrics_seconds = start.elapsed().as_secs_f64();
        let start = Instant::now();
        let after = sources
            .iter()
            .map(|s| collect(&s.path, &s.node, window, started).unwrap())
            .collect::<Vec<_>>();
        let collect_seconds = start.elapsed().as_secs_f64();
        assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(after).unwrap()
        );
        let sql = "SELECT kind, COUNT(*) AS events FROM state_events GROUP BY kind ORDER BY kind";
        let start = Instant::now();
        let uncached = load(&sources, window, started).unwrap();
        let expected = rows(&uncached, sql);
        let uncached_seconds = start.elapsed().as_secs_f64();
        let start = Instant::now();
        let prepared = load_cached(temp.path(), &sources, window, started).unwrap();
        assert_eq!(
            Path::new(prepared.connection.path().unwrap())
                .canonicalize()
                .unwrap(),
            database(temp.path()).canonicalize().unwrap()
        );
        assert_eq!(rows(&prepared, sql), expected);
        let first_seconds = start.elapsed().as_secs_f64();
        let start = Instant::now();
        let again = load_cached(temp.path(), &sources, window, started).unwrap();
        assert_eq!(rows(&again, sql), expected);
        let repeat_seconds = start.elapsed().as_secs_f64();
        assert_eq!(
            (prepared.invalid_lines, prepared.dropped),
            (uncached.invalid_lines, uncached.dropped)
        );
        eprintln!(
            "{}",
            serde_json::json!({
                "run": manifest.id, "events": prepared.events,
                "metrics_only_seconds": metrics_seconds, "metrics_and_cache_seconds": collect_seconds,
                "added_collection_seconds": collect_seconds - metrics_seconds,
                "uncached_query_seconds": uncached_seconds, "prepared_first_query_seconds": first_seconds,
                "repeated_query_seconds": repeat_seconds, "cache_bytes": fs::metadata(database(temp.path())).unwrap().len(),
            })
        );
    }
}
