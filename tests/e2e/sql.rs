use super::*;
use serde_json::Value;

fn isuscope(project: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(args)
        .current_dir(project)
        .output()
        .unwrap()
}

#[test]
fn sql_reads_the_index_and_refuses_to_change_it() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"metric\",\"name\":\"benchmark.viewers\",\"value\":3}' '{\"type\":\"isuscope.result\",\"pass\":true,\"score\":42}'"]
"#,
    )
    .unwrap();
    assert!(
        isuscope(
            project.path(),
            &["run", "--hypothesis", "index is queryable"]
        )
        .status
        .success()
    );

    let schema = isuscope(project.path(), &["sql", "--schema"]);
    let schema = String::from_utf8_lossy(&schema.stdout);
    assert!(schema.contains("CREATE TABLE runs"), "{schema}");
    assert!(schema.contains("CREATE TABLE metrics"), "{schema}");

    let rows = isuscope(
        project.path(),
        &["sql", "SELECT score, state FROM runs ORDER BY started_at"],
    );
    let rows: Value = serde_json::from_slice(&rows.stdout).unwrap();
    assert_eq!(rows["row_count"], 1);
    assert_eq!(rows["rows"][0]["score"], 42);
    assert_eq!(rows["rows"][0]["state"], "complete");
    assert_eq!(rows["truncated"], false);

    // The limit says so instead of printing everything.
    let limited = isuscope(
        project.path(),
        &[
            "sql",
            "SELECT name FROM metrics UNION ALL SELECT name FROM metrics",
            "--limit",
            "1",
            "--format",
            "tsv",
        ],
    );
    let limited = String::from_utf8_lossy(&limited.stdout);
    assert!(limited.starts_with("name\n"), "{limited}");
    assert!(limited.contains("# truncated at 1 rows"), "{limited}");

    // A write is refused by the read-only connection, and the run survives.
    let write = isuscope(project.path(), &["sql", "DELETE FROM runs"]);
    assert!(!write.status.success());
    assert!(
        String::from_utf8_lossy(&write.stderr).contains("readonly"),
        "{}",
        String::from_utf8_lossy(&write.stderr)
    );
    let after = isuscope(
        project.path(),
        &["sql", "SELECT COUNT(*) AS runs FROM runs"],
    );
    let after: Value = serde_json::from_slice(&after.stdout).unwrap();
    assert_eq!(after["rows"][0]["runs"], 1);

    // The removed commands are gone; ui still renders those views.
    assert!(!isuscope(project.path(), &["report"]).status.success());
    assert!(
        !isuscope(project.path(), &["diff", "latest", "latest"])
            .status
            .success()
    );
}
