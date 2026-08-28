use super::*;

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
