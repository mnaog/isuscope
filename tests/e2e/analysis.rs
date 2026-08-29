use super::*;

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
            "skipped",
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
    let (analyzed_run, hypothesis, body): (String, String, String) = database
        .query_row(
            "SELECT r.id, r.hypothesis, a.body FROM runs r JOIN run_analyses a ON a.run_id=r.id WHERE a.verdict='supported'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
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
        .args(["report", &analyzed_run])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(restored.status.success());
    let report: serde_json::Value = serde_json::from_slice(&restored.stdout).unwrap();
    assert_eq!(
        report["run"]["hypothesis"],
        "removing one allocation raises score without errors"
    );
    assert_eq!(report["run"]["analyses"][1]["verdict"], "inconclusive");
    assert_eq!(
        report["run"]["analyses"][1]["body"],
        "One sample is insufficient; retain the change provisionally."
    );
    let restored = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        restored
            .query_row("SELECT COUNT(*) FROM run_analyses", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
}
