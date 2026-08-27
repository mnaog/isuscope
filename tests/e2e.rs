use rusqlite::Connection;
use std::{fs, process::Command};
use tempfile::tempdir;

#[cfg(unix)]
#[test]
fn signal_finalizes_aborted_run_and_executes_after_cleanup() {
    use std::{thread, time::Duration};

    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-started; printf 'webappの初期化を行います\n'; sleep 30"]

[[collectors]]
name = "cleanup"
phase = "after"
transport = "local"
command = ["sh", "-c", "printf cleaned > cleanup-ran"]
"#,
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "signal interruption is finalized safely",
        ])
        .current_dir(project.path())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if project.path().join("benchmark-started").is_file() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(project.path().join("benchmark-started").is_file());
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(child.wait().unwrap().code(), Some(1));
    assert_eq!(
        fs::read_to_string(project.path().join("cleanup-ran")).unwrap(),
        "cleaned"
    );
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (state, passed): (String, bool) = database
        .query_row("SELECT state, passed FROM runs", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(state, "aborted");
    assert!(!passed);
    assert_eq!(
        fs::read_dir(config_dir.join("runs/.incomplete"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn init_is_non_interactive_and_preserves_existing_files() {
    let project = tempdir().unwrap();
    let first = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("init")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(first.status.success());
    let config = project.path().join(".isuscope/config.toml");
    assert!(config.is_file());
    assert!(project.path().join(".isuscope/setup.sh").is_file());
    let benchmark = project.path().join(".isuscope/benchmark.sh");
    assert!(benchmark.is_file());
    let benchmark_parser = project.path().join(".isuscope/parse-benchmark.sh");
    assert!(benchmark_parser.is_file());
    assert!(
        fs::read_to_string(&config)
            .unwrap()
            .contains(".isuscope/benchmark.sh")
    );
    let generated = fs::read_to_string(&config).unwrap();
    assert!(generated.contains("# [context.codex]"));
    assert!(generated.contains("# history_dir = \"docs/codex-history\""));
    assert!(generated.contains("$log.$1"));
    assert!(generated.contains("failed fingerprint validation"));
    assert!(generated.contains("gzip -t"));
    let parsed: toml::Value = toml::from_str(&generated).unwrap();
    let collectors = parsed["collectors"].as_array().unwrap();
    for name in [
        "perf-start",
        "perf-stop",
        "perf-report",
        "perf-series",
        "nginx-log-delta",
        "alp",
        "mysql-log-delta",
        "slp",
    ] {
        let collector = collectors
            .iter()
            .find(|collector| collector["name"].as_str() == Some(name))
            .unwrap();
        let command = collector["command"].as_array().unwrap();
        let script = command[2].as_str().unwrap();
        assert!(
            Command::new("sh")
                .args(["-n", "-c", script])
                .status()
                .unwrap()
                .success(),
            "invalid shell syntax in {name}"
        );
    }
    for name in [
        "sysstat",
        "perf-start",
        "perf-stop",
        "perf-report",
        "perf-series",
        "nginx-log-mark",
        "nginx-log-delta",
        "alp",
        "mysql-log-mark",
        "mysql-log-delta",
        "slp",
    ] {
        assert!(
            collectors
                .iter()
                .any(|collector| collector["name"].as_str() == Some(name))
        );
    }
    assert!(
        fs::read_to_string(&benchmark)
            .unwrap()
            .contains("isuscope benchmark adapter protocol v1")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            fs::metadata(&benchmark).unwrap().permissions().mode() & 0o111,
            0
        );
        assert_ne!(
            fs::metadata(&benchmark_parser)
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
    fs::write(&config, "user-owned\n").unwrap();
    fs::write(&benchmark, "user-owned benchmark adapter\n").unwrap();

    let second = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("init")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(second.status.success());
    assert_eq!(fs::read_to_string(config).unwrap(), "user-owned\n");
    assert_eq!(
        fs::read_to_string(benchmark).unwrap(),
        "user-owned benchmark adapter\n"
    );
}

#[cfg(unix)]
#[test]
fn standard_log_delta_survives_common_rotation_strategies() {
    let config: toml::Value = toml::from_str(include_str!("../templates/config.toml")).unwrap();
    let collectors = config["collectors"].as_array().unwrap();
    let script = |name: &str| {
        collectors
            .iter()
            .find(|collector| collector["name"].as_str() == Some(name))
            .unwrap()["command"][2]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let mark_template = script("nginx-log-mark");
    let delta_template = script("nginx-log-delta");

    let run_case = |name: &str, rotate: &dyn Fn(&std::path::Path)| {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("access.log");
        fs::write(&log, b"before\n").unwrap();
        let prefix = directory.path().join(format!("isuscope-{name}"));
        let prepare = |template: &str| {
            template
                .replace("/var/log/nginx/access.log", log.to_str().unwrap())
                .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
        };
        let mark = Command::new("sh")
            .args(["-c", &prepare(&mark_template)])
            .output()
            .unwrap();
        assert!(
            mark.status.success(),
            "mark failed for {name}: {}",
            String::from_utf8_lossy(&mark.stderr)
        );
        rotate(&log);
        let delta = Command::new("sh")
            .args(["-c", &prepare(&delta_template)])
            .output()
            .unwrap();
        assert!(
            delta.status.success(),
            "delta failed for {name}: {}",
            String::from_utf8_lossy(&delta.stderr)
        );
        delta.stdout
    };

    assert_eq!(
        run_case("append", &|log| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(log)
                .unwrap()
                .write_all(b"appended\n")
                .unwrap();
        }),
        b"appended\n"
    );
    assert_eq!(
        run_case("rename", &|log| {
            fs::rename(log, format!("{}.1", log.display())).unwrap();
            fs::write(log, b"new\n").unwrap();
        }),
        b"new\n"
    );
    assert_eq!(
        run_case("copytruncate", &|log| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(log)
                .unwrap()
                .write_all(b"old-tail\n")
                .unwrap();
            fs::copy(log, format!("{}.1", log.display())).unwrap();
            fs::write(log, b"new\n").unwrap();
        }),
        b"old-tail\nnew\n"
    );
    assert_eq!(
        run_case("gzip", &|log| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(log)
                .unwrap()
                .write_all(b"old-tail\n")
                .unwrap();
            let rotated = format!("{}.1", log.display());
            fs::rename(log, &rotated).unwrap();
            assert!(
                Command::new("gzip")
                    .arg(&rotated)
                    .status()
                    .unwrap()
                    .success()
            );
            fs::write(log, b"new\n").unwrap();
        }),
        b"old-tail\nnew\n"
    );
    assert_eq!(
        run_case("multiple", &|log| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(log)
                .unwrap()
                .write_all(b"first-tail\n")
                .unwrap();
            fs::rename(log, format!("{}.1", log.display())).unwrap();
            fs::write(log, b"second\n").unwrap();
            fs::rename(
                format!("{}.1", log.display()),
                format!("{}.2", log.display()),
            )
            .unwrap();
            fs::rename(log, format!("{}.1", log.display())).unwrap();
            fs::write(log, b"third\n").unwrap();
        }),
        b"first-tail\nsecond\nthird\n"
    );
}

#[test]
fn passing_runs_require_hypothesis_and_analysis_before_the_next_benchmark() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf 'x' >> benchmark-ran; printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":100,\"pass\":true}'"]
"#,
    )
    .unwrap();

    let missing = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("run")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(!project.path().join("benchmark-ran").exists());

    let first = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "removing one allocation raises score without errors",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(first.status.success());
    assert!(String::from_utf8_lossy(&first.stdout).contains("analysis  pending"));
    assert_eq!(
        fs::read_to_string(project.path().join("benchmark-ran")).unwrap(),
        "x"
    );

    let blocked = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "a second unreviewed change helps"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("awaiting analysis"));
    assert_eq!(
        fs::read_to_string(project.path().join("benchmark-ran")).unwrap(),
        "x"
    );

    let analysis = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "analyze",
            "latest",
            "--verdict",
            "supported",
            "--analysis",
            "Score reached 100 and the benchmark passed.",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(analysis.status.success());

    let revision = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "analyze",
            "latest",
            "--verdict",
            "inconclusive",
            "--analysis",
            "One sample is insufficient; retain the change provisionally.",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(revision.status.success());
    assert!(String::from_utf8_lossy(&revision.stdout).contains("revisions 2"));

    let second = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "the follow-up change raises score"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(second.status.success());
    assert_eq!(
        fs::read_to_string(project.path().join("benchmark-ran")).unwrap(),
        "xx"
    );

    let skipped = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "analyze",
            "latest",
            "--skip",
            "--reason",
            "practice window ended before analysis",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(skipped.status.success());

    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf 'x' >> benchmark-ran; printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":0,\"pass\":false}'"]
"#,
    )
    .unwrap();
    let failed_after_skip = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "the intentionally failing control is retained",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_eq!(failed_after_skip.status.code(), Some(1));
    assert_eq!(
        fs::read_to_string(project.path().join("benchmark-ran")).unwrap(),
        "xxx"
    );

    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let rows: (i64, i64, i64, i64) = database
        .query_row(
            "SELECT COUNT(*), SUM(analysis_status='complete'), SUM(analysis_status='skipped'), SUM(analysis_status='not_required') FROM runs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(rows, (3, 1, 1, 1));
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM run_analyses", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
    let (hypothesis, body): (String, String) = database
        .query_row(
            "SELECT r.hypothesis, a.body FROM runs r JOIN run_analyses a ON a.run_id=r.id WHERE a.verdict='supported'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        hypothesis,
        "removing one allocation raises score without errors"
    );
    assert!(body.contains("reached 100"));
    drop(database);

    for suffix in ["", "-wal", "-shm"] {
        let path = config_dir.join(format!("isuscope.sqlite3{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let restored = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("show")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(restored.status.success());
    let restored_output = String::from_utf8(restored.stdout).unwrap();
    assert!(restored_output.contains("removing one allocation raises score without errors"));
    assert!(restored_output.contains("inconclusive: One sample is insufficient"));
    let restored = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        restored
            .query_row("SELECT COUNT(*) FROM run_analyses", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
}

#[test]
fn discovery_run_persists_score_metrics_transitions_and_logs() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
data_dir = ".isuscope"

[source]
repo = "."

[benchmark]
mode = "command"
command = ["sh", "-c", "test \"$ISUSCOPE_BENCHMARK_PROTOCOL\" = v1; test \"$ISUSCOPE_PROJECT_ROOT\" = \"$PWD\"; test -f \"$ISUSCOPE_RUN_DIR/run.json\"; printf 'webappの初期化を行います\nベンチマーク走行前のデータ整合性チェックを行います\nスコア: 12345\n'"]

[[collectors]]
name = "calculated"
phase = "after"
transport = "local"
modes = ["discovery-run"]
command = ["sh", "-c", "grep -q '\"started_at\":' '{run_dir}/run.json'; printf '%s\\n' '{\"type\":\"metric\",\"name\":\"cpu\",\"value\":12.5,\"unit\":\"percent\",\"timestamp\":\"2026-08-27T12:34:56.789Z\"}' '{\"type\":\"fingerprint\",\"name\":\"app.binary.sha256\",\"value\":\"abc123\"}' '{\"type\":\"transition\",\"from\":\"GET /a\",\"to\":\"GET /b\",\"count\":7}'"]
"#,
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "discovery-run",
            "--hypothesis",
            "collectors preserve benchmark evidence",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8(run.stdout).unwrap();
    assert!(stdout.contains("score     12345"));
    assert!(stdout.contains("result    PASS"));

    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (score, passed): (i64, bool) = database
        .query_row("SELECT score, passed FROM runs", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(score, 12345);
    assert!(passed);
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    let labels: String = database
        .query_row(
            "SELECT labels_json FROM metrics WHERE name='cpu'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(labels.contains("\"collector\":\"calculated\""));
    assert_eq!(
        database
            .query_row(
                "SELECT observed_at FROM metrics WHERE name='cpu'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "2026-08-27T12:34:56.789+00:00"
    );
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM transitions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM fingerprints", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM logs", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        4
    );

    let show = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(show.status.success());
    let show_stdout = String::from_utf8(show.stdout).unwrap();
    assert!(show_stdout.contains("score       12345"));
    assert!(show_stdout.contains("transitions 1"));
    assert!(show_stdout.contains("fingerprints 1"));
    assert!(show_stdout.contains("observability"));
    assert!(show_stdout.contains("complete     calculated"));
    assert!(show_stdout.contains("metric series"));
    assert!(show_stdout.contains("cpu"));
    assert!(show_stdout.contains("sqlite      "));
    assert!(show_stdout.contains("(shared by all runs)"));
    assert!(show_stdout.contains("sql hint    sqlite3 "));
    assert!(show_stdout.contains("FROM metrics WHERE run_id="));
    assert!(show_stdout.contains("compare     sqlite3 "));
    assert!(show_stdout.contains("view    zstd -dc -- "));

    let runs = config_dir.join("runs");
    let run_dir = fs::read_dir(&runs)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() != ".incomplete")
        .unwrap()
        .path();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(run_dir.join("run.json")).unwrap()).unwrap();
    database
        .execute(
            "UPDATE metrics SET observed_at=?1 WHERE name='cpu'",
            [manifest["benchmark"]["started_at"].as_str().unwrap()],
        )
        .unwrap();
    let series = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["series", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(series.status.success());
    let series_stdout = String::from_utf8(series.stdout).unwrap();
    assert!(series_stdout.contains("bucket 5s"));
    assert!(series_stdout.contains("CPU A/M%"));
    assert!(series_stdout.contains("0-"));

    let metrics = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["metrics", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(metrics.status.success());
    let metrics_stdout = String::from_utf8(metrics.stdout).unwrap();
    assert!(metrics_stdout.contains("NAME"));
    assert!(metrics_stdout.contains("cpu"));
    assert!(metrics_stdout.contains("label collector"));
    assert!(metrics_stdout.contains("calculated"));

    let filtered = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "series",
            "latest",
            "--metric",
            "cpu",
            "--label",
            "collector=calculated",
            "--bucket",
            "10",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(filtered.status.success());
    let filtered_stdout = String::from_utf8(filtered.stdout).unwrap();
    assert!(filtered_stdout.contains("bucket 10s"));
    assert!(filtered_stdout.contains("cpu"));
    assert!(filtered_stdout.contains("12.500"));
    assert!(filtered_stdout.contains("average"));
    assert!(filtered_stdout.contains("collector=calculated"));
    assert!(run_dir.join("tooling/config.toml").is_file());
    assert!(run_dir.join("tooling/isuscope-version.txt").is_file());
    assert_eq!(manifest["schema_version"], 6);
    assert_eq!(
        manifest["hypothesis"],
        "collectors preserve benchmark evidence"
    );
    assert_eq!(manifest["analysis_status"], "pending");
    assert_eq!(manifest["tooling"]["isuscope_version"], "0.8.0");
    assert_eq!(
        manifest["tooling"]["config_sha256"].as_str().unwrap().len(),
        64
    );

    drop(database);
    for suffix in ["", "-wal", "-shm"] {
        let path = config_dir.join(format!("isuscope.sqlite3{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let restored_show = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        restored_show.status.success(),
        "{}",
        String::from_utf8_lossy(&restored_show.stderr)
    );
    assert!(String::from_utf8_lossy(&restored_show.stderr).contains("reindexed"));
    let restored = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        restored
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        restored
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        restored
            .query_row("SELECT COUNT(*) FROM transitions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        restored
            .query_row("SELECT COUNT(*) FROM fingerprints", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn enrich_replaces_parser_metrics_and_annotations_are_queryable() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let write_config = |value: i64| {
        fs::write(
            config_dir.join("config.toml"),
            format!(
                r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf 'viewer completed: 10\nscore: 123\n'"]
score_pattern = "score: ([0-9]+)"

[[benchmark.parsers]]
name = "contest-output"
command = ["sh", "-c", "printf '%s\\n' '{{\"type\":\"metric\",\"name\":\"benchmark.viewer.completed\",\"value\":{value},\"unit\":\"viewers\",\"timestamp\":\"2026-08-27T12:34:56Z\"}}'"]
"#,
            ),
        )
        .unwrap();
    };
    write_config(10);

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "parser emits a queryable viewer metric",
            "--note",
            "initial parser",
            "--tag",
            "candidate",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (note, value, labels): (String, f64, String) = database
        .query_row(
            "SELECT r.note, m.value, m.labels_json FROM runs r JOIN metrics m ON m.run_id=r.id WHERE m.name='benchmark.viewer.completed'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(note, "initial parser");
    assert_eq!(value, 10.0);
    assert!(labels.contains("isuscope.parser"));
    assert_eq!(
        database
            .query_row(
                "SELECT COUNT(*) FROM run_tags WHERE tag='candidate'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    drop(database);

    write_config(20);
    let enrich = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["enrich", "candidate"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        enrich.status.success(),
        "{}",
        String::from_utf8_lossy(&enrich.stderr)
    );
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (count, value): (i64, f64) = database
        .query_row(
            "SELECT COUNT(*), MAX(value) FROM metrics WHERE name='benchmark.viewer.completed'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(value, 20.0);
    assert_eq!(
        database
            .query_row(
                "SELECT observed_at FROM metrics WHERE name='benchmark.viewer.completed'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "2026-08-27T12:34:56+00:00"
    );
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM enrichment_runs", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(database);

    let annotate = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "annotate",
            "candidate",
            "--note",
            "parser updated",
            "--tag",
            "baseline",
            "--remove-tag",
            "candidate",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(annotate.status.success());
    let show = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "baseline"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(show.status.success());
    let output = String::from_utf8(show.stdout).unwrap();
    assert!(output.contains("note        parser updated"));
    assert!(output.contains("tags        baseline"));

    for suffix in ["", "-wal", "-shm"] {
        let path = config_dir.join(format!("isuscope.sqlite3{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let restored = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "baseline"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(restored.status.success());
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        database
            .query_row(
                "SELECT value FROM metrics WHERE name='benchmark.viewer.completed'",
                [],
                |row| row.get::<_, f64>(0)
            )
            .unwrap(),
        20.0
    );
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM enrichment_runs", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn adapter_can_emit_metrics_without_an_external_parser() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"metric\",\"name\":\"benchmark.viewer.completed\",\"value\":77,\"unit\":\"viewers\"}' '{\"type\":\"isuscope.result\",\"score\":456,\"pass\":true}'"]
"#,
    )
    .unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "inline adapter metrics are stored"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(run.status.success());
    let run_dir = fs::read_dir(config_dir.join("runs"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_dir() && path.file_name().unwrap() != ".incomplete")
        .unwrap();
    assert!(run_dir.join("structured.json.zst").is_file());
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (value, labels): (f64, String) = database
        .query_row(
            "SELECT value, labels_json FROM metrics WHERE name='benchmark.viewer.completed'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(value, 77.0);
    assert!(labels.contains("inline"));
    drop(database);
    for suffix in ["", "-wal", "-shm"] {
        let path = config_dir.join(format!("isuscope.sqlite3{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let restored = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(restored.status.success());
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        database
            .query_row(
                "SELECT value FROM metrics WHERE name='benchmark.viewer.completed'",
                [],
                |row| row.get::<_, f64>(0)
            )
            .unwrap(),
        77.0
    );
}

#[test]
fn score_run_skips_collectors_and_benchmark_parsers() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' 'webappの初期化を行います' 'ベンチマーク走行前のデータ整合性チェック' '{\"type\":\"isuscope.result\",\"score\":321,\"pass\":true}'"]

[[benchmark.parsers]]
name = "must-not-run"
command = ["sh", "-c", "touch parser-ran"]

[[collectors]]
name = "must-not-run"
phase = "after"
command = ["sh", "-c", "touch collector-ran"]
"#,
    )
    .unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "score-run",
            "--hypothesis",
            "score mode omits observation overhead",
            "--tag",
            "final",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(!project.path().join("parser-ran").exists());
    assert!(!project.path().join("collector-ran").exists());
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (mode, score, collectors, metrics): (String, i64, i64, i64) = database
        .query_row(
            "SELECT mode, score, (SELECT COUNT(*) FROM collector_runs), (SELECT COUNT(*) FROM metrics) FROM runs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(mode, "score-run");
    assert_eq!(score, 321);
    assert_eq!(collectors, 0);
    assert_eq!(metrics, 0);
    drop(database);
    for suffix in ["", "-wal", "-shm"] {
        let path = config_dir.join(format!("isuscope.sqlite3{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let restored = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "final"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(restored.status.success());
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn doctor_checks_without_starting_benchmark() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("check.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[tooling]
include = ["check.sh"]

[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-ran"]
"#,
    )
    .unwrap();
    let doctor = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("doctor")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert!(!project.path().join("benchmark-ran").exists());
    let output = String::from_utf8(doctor.stdout).unwrap();
    assert!(output.contains("failures  0"));
    assert!(output.contains("no SSH nodes are configured"));
}

#[test]
fn codex_context_is_required_snapshotted_and_indexed_when_configured() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    let history_dir = project.path().join("docs/codex-history");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&history_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[context.codex]
history_dir = "docs/codex-history"

[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-ran; printf '%s\\n' '{\"type\":\"isuscope.result\",\"pass\":true,\"score\":42}'"]
"#,
    )
    .unwrap();
    let history = r#"# Codex conversation

- Session: `session-a`

<!-- codex-event:session-a:turn-old:user -->
## [User] 2026-08-27T20:00:00+09:00

old request

<!-- codex-event:session-a:turn-old:codex -->
## [Codex] 2026-08-27T20:00:01+09:00

old reply

<!-- codex-event:session-a:turn-current:user -->
## [User] 2026-08-27T20:01:00+09:00

run this benchmark
"#;
    fs::write(history_dir.join("20260827-200000.md"), history).unwrap();

    let doctor = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("doctor")
        .env("CODEX_SESSION_ID", "session-a")
        .env("CODEX_THREAD_ID", "session-a")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(doctor.status.success());
    assert!(
        String::from_utf8_lossy(&doctor.stdout)
            .contains("Codex context resolved: docs/codex-history/20260827-200000.md#turn-current")
    );

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "context is linked automatically"])
        .env("CODEX_SESSION_ID", "session-a")
        .env("CODEX_THREAD_ID", "session-a")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(project.path().join("benchmark-ran").is_file());

    let run_dir = fs::read_dir(config_dir.join("runs"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() != ".incomplete")
        .unwrap()
        .path();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(run_dir.join("run.json")).unwrap()).unwrap();
    assert_eq!(
        manifest["codex_context"]["history_path"],
        "docs/codex-history/20260827-200000.md"
    );
    assert_eq!(manifest["codex_context"]["session_id"], "session-a");
    assert_eq!(manifest["codex_context"]["input_id"], "turn-current");
    assert_eq!(
        fs::read_to_string(run_dir.join("context/codex-history.md")).unwrap(),
        history
    );
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let indexed: (String, String, String) = database
        .query_row(
            "SELECT history_path, session_id, input_id FROM run_codex_context",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        indexed,
        (
            "docs/codex-history/20260827-200000.md".into(),
            "session-a".into(),
            "turn-current".into()
        )
    );
    let show = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    let output = String::from_utf8_lossy(&show.stdout);
    assert!(output.contains("codex file  docs/codex-history/20260827-200000.md"));
    assert!(output.contains("codex input turn-current"));
    drop(database);
    for suffix in ["", "-wal", "-shm"] {
        let path = config_dir.join(format!("isuscope.sqlite3{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let restored = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(restored.status.success());
    assert!(String::from_utf8_lossy(&restored.stderr).contains("reindexed"));
    let restored = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        restored
            .query_row("SELECT input_id FROM run_codex_context", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "turn-current"
    );
}

#[test]
fn codex_context_rejects_a_run_without_the_current_session() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    let history_dir = project.path().join("docs/codex-history");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&history_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[context.codex]
history_dir = "docs/codex-history"

[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-ran"]
"#,
    )
    .unwrap();
    fs::write(
        history_dir.join("20260827-200000.md"),
        "# Codex conversation\n\n- Session: `session-a`\n\n<!-- codex-event:session-a:turn-a:user -->\n",
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "this must never start"])
        .env_remove("CODEX_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr)
            .contains("CODEX_SESSION_ID/CODEX_THREAD_ID is not set")
    );
    assert!(!project.path().join("benchmark-ran").exists());

    let wrong_session = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "wrong session must never start"])
        .env("CODEX_SESSION_ID", "session-b")
        .env("CODEX_THREAD_ID", "session-b")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_eq!(wrong_session.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&wrong_session.stderr)
            .contains("belongs to current session `session-b`")
    );
    assert!(!project.path().join("benchmark-ran").exists());
}

#[test]
fn failed_initialize_is_still_finalized() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[source]
repo = "."

[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\\n' '{\"type\":\"isuscope.result\",\"pass\":false,\"score\":0,\"messages\":[\"initialize failed\"]}'; exit 1"]
"#,
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "initialize failure is retained"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));

    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (score, passed, state): (i64, bool, String) = database
        .query_row("SELECT score, passed, state FROM runs", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(score, 0);
    assert!(!passed);
    assert_eq!(state, "failed");

    let run_count = fs::read_dir(config_dir.join("runs"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != ".incomplete")
        .count();
    assert_eq!(run_count, 1);
}

#[test]
fn bottleneck_prints_five_request_weighted_routes() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let mut records = Vec::new();
    for index in 1..=6 {
        records.push(format!(
            "{{\"type\":\"metric\",\"name\":\"http.requests\",\"value\":{index},\"labels\":{{\"node\":\"isu1\",\"method\":\"GET\",\"route\":\"/{index}\",\"status_class\":\"2xx\"}}}}"
        ));
        records.push(format!(
            "{{\"type\":\"metric\",\"name\":\"http.request_duration\",\"value\":10,\"unit\":\"ms\",\"labels\":{{\"node\":\"isu1\",\"method\":\"GET\",\"route\":\"/{index}\",\"quantile\":\"0.95\"}}}}"
        ));
    }
    fs::write(
        config_dir.join("metrics.jsonl"),
        format!("{}\n", records.join("\n")),
    )
    .unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf 'スコア: 1\n'"]

[[collectors]]
name = "routes"
phase = "after"
transport = "local"
command = ["cat", ".isuscope/metrics.jsonl"]
"#,
    )
    .unwrap();
    assert!(
        Command::new(env!("CARGO_BIN_EXE_isuscope"))
            .args([
                "discovery-run",
                "--hypothesis",
                "request weights expose the leading routes",
            ])
            .current_dir(project.path())
            .status()
            .unwrap()
            .success()
    );

    let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("bottleneck")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("one leader per observed category"));
    assert!(stdout.contains("not a cross-category remediation priority"));
    assert!(stdout.contains("1     http        isu1          GET /6"));
    assert!(stdout.contains("http       complete"));
    assert!(stdout.contains("database   unavailable"));
    assert!(!stdout.contains("/1"));
}

#[test]
fn unavailable_optional_collector_does_not_degrade_run() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf 'スコア: 1\n'"]

[[collectors]]
name = "retired-mysql"
phase = "after"
command = ["sh", "-c", "exit 75"]
"#,
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "unavailable optional collection does not degrade the run",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(run.status.success());
    assert!(String::from_utf8_lossy(&run.stdout).contains("retired-mysql (local, unavailable)"));
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        database
            .query_row("SELECT state FROM runs", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "complete"
    );
    assert_eq!(
        database
            .query_row("SELECT status FROM collector_runs", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "unavailable"
    );
}

#[test]
fn nonzero_adapter_exit_cannot_report_a_passing_run() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\\n' '{\"type\":\"isuscope.result\",\"score\":999,\"pass\":true}'; exit 7"]
"#,
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "a nonzero adapter exit overrides a reported pass",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));

    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (score, passed, state): (i64, bool, String) = database
        .query_row("SELECT score, passed, state FROM runs", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(score, 999);
    assert!(!passed);
    assert_eq!(state, "failed");
}
