use super::*;

#[test]
fn query_reads_sqlite_metrics_and_groups_database_shapes_safely() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\\n' '{\"type\":\"metric\",\"name\":\"benchmark.scenario.success\",\"value\":10,\"unit\":\"runs\",\"labels\":{\"scenario\":\"viewer\"}}' '{\"type\":\"metric\",\"name\":\"benchmark.scenario.failure\",\"value\":2,\"unit\":\"runs\",\"labels\":{\"scenario\":\"viewer\"}}' '{\"type\":\"metric\",\"name\":\"db.query.calls\",\"value\":4,\"unit\":\"queries\",\"labels\":{\"collector\":\"mysql-log-delta\",\"engine\":\"mysql\",\"node\":\"app1\",\"digest\":\"update reservation_slots set slot = slot - ? where id in (?)\"}}' '{\"type\":\"metric\",\"name\":\"db.query.total_duration\",\"value\":8,\"unit\":\"ms\",\"labels\":{\"collector\":\"mysql-log-delta\",\"engine\":\"mysql\",\"node\":\"app1\",\"digest\":\"update reservation_slots set slot = slot - ? where id in (?)\"}}' '{\"type\":\"metric\",\"name\":\"db.query.p95_duration\",\"value\":3,\"unit\":\"ms\",\"labels\":{\"collector\":\"mysql-log-delta\",\"engine\":\"mysql\",\"node\":\"app1\",\"digest\":\"update reservation_slots set slot = slot - ? where id in (?)\"}}' '{\"type\":\"metric\",\"name\":\"db.query.calls\",\"value\":6,\"unit\":\"queries\",\"labels\":{\"collector\":\"mysql-log-delta\",\"engine\":\"mysql\",\"node\":\"app1\",\"digest\":\"update reservation_slots set slot = slot - ? where id in (?,?)\"}}' '{\"type\":\"metric\",\"name\":\"db.query.total_duration\",\"value\":18,\"unit\":\"ms\",\"labels\":{\"collector\":\"mysql-log-delta\",\"engine\":\"mysql\",\"node\":\"app1\",\"digest\":\"update reservation_slots set slot = slot - ? where id in (?,?)\"}}' '{\"type\":\"metric\",\"name\":\"db.query.p95_duration\",\"value\":5,\"unit\":\"ms\",\"labels\":{\"collector\":\"mysql-log-delta\",\"engine\":\"mysql\",\"node\":\"app1\",\"digest\":\"update reservation_slots set slot = slot - ? where id in (?,?)\"}}' '{\"type\":\"isuscope.result\",\"score\":123,\"pass\":true}'"]
"#,
    )
    .unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "query e2e"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let scenarios = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "query",
            "latest",
            "--metric-prefix",
            "benchmark.scenario.",
            "--group-by",
            "scenario",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(scenarios.status.success());
    let scenarios: serde_json::Value = serde_json::from_slice(&scenarios.stdout).unwrap();
    assert_eq!(scenarios["total_count"], 2);
    assert_eq!(scenarios["rows"][0]["value"], 2.0);
    assert_eq!(scenarios["rows"][1]["value"], 10.0);

    let database = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "query",
            "latest",
            "--view",
            "database",
            "--source",
            "mysql-log-delta",
            "--label-contains",
            "digest=reservation_slots",
            "--group-by",
            "sql-shape",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(database.status.success());
    let database: serde_json::Value = serde_json::from_slice(&database.stdout).unwrap();
    assert_eq!(database["total_count"], 1);
    assert_eq!(database["rows"][0]["digest_count"], 2);
    assert_eq!(database["rows"][0]["calls"], 10.0);
    assert_eq!(database["rows"][0]["total_ms"], 26.0);
    assert_eq!(database["rows"][0]["avg_ms"], 2.6);
    assert!(database["rows"][0]["p95_ms"].is_null());
    assert_eq!(
        database["rows"][0]["unavailable"]["p95_ms"],
        "scalar quantiles cannot be merged across digests"
    );

    let database_diff = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "query",
            "latest",
            "--base",
            "latest",
            "--view",
            "database",
            "--source",
            "mysql-log-delta",
            "--label-contains",
            "digest=reservation_slots",
            "--group-by",
            "sql-shape",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(database_diff.status.success());
    let database_diff: serde_json::Value = serde_json::from_slice(&database_diff.stdout).unwrap();
    assert_eq!(database_diff["view"], "database");
    assert_eq!(database_diff["total_count"], 1);
    assert_eq!(database_diff["rows"][0]["presence"], "both");
    assert_eq!(
        database_diff["rows"][0]["changes"]["total_ms"]["delta"],
        0.0
    );

    let brief = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["brief", "latest", "--limit", "1"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(brief.status.success());
    let brief: serde_json::Value = serde_json::from_slice(&brief.stdout).unwrap();
    assert_eq!(brief["run"]["score"], 123);
    assert_eq!(brief["run"]["passed"], true);
    assert_eq!(brief["benchmark"]["total_count"], 2);
    assert_eq!(brief["benchmark"]["truncated"], true);
    assert_eq!(brief["benchmark"]["items"].as_array().unwrap().len(), 1);
}
