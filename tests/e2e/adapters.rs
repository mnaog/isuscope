use super::*;

#[test]
fn score_run_is_not_a_public_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["score-run", "--help"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand"));
}

#[test]
fn discovery_run_is_not_a_public_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["discovery-run", "--help"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand"));
}

#[test]
fn mutating_commands_require_an_explicit_run() {
    for command in ["enrich", "analyze"] {
        let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
            .arg(command)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{command} accepted an implicit run"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("<RUN>"));
    }
}

#[test]
fn diff_requires_both_runs() {
    let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["diff", "base-only"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("<CANDIDATE>"));
}

#[test]
fn analyze_rejects_the_removed_flag_style_verdict() {
    let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["analyze", "run-id", "--verdict", "supported"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument '--verdict'"));
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
        .arg("list")
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
