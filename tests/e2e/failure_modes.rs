use super::*;

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
