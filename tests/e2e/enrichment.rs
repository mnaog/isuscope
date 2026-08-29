use super::*;

#[test]
fn enrich_replaces_parser_metrics_and_survives_reindexing() {
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

    let removed_annotate = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["annotate", "candidate"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!removed_annotate.status.success());
    let report = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["report", "candidate"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(report.status.success());
    let report: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["run"]["note"], "initial parser");
    assert_eq!(report["run"]["tags"], serde_json::json!(["candidate"]));

    for suffix in ["", "-wal", "-shm"] {
        let path = config_dir.join(format!("isuscope.sqlite3{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let restored = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["report", "candidate"])
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
