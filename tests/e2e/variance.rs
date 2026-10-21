use super::*;
use serde_json::Value;

fn isuscope(project: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(args)
        .current_dir(project)
        .output()
        .unwrap()
}

fn ok(project: &std::path::Path, args: &[&str]) -> Value {
    let output = isuscope(project, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    parsed(&output.stdout).unwrap_or_default()
}

fn measure(project: &std::path::Path, score: i64, repeat: Option<&str>) {
    fs::write(project.join("score"), score.to_string()).unwrap();
    let mut args = vec!["run", "--hypothesis", "measure"];
    if let Some(reason) = repeat {
        args.extend(["--repeat", reason]);
    }
    ok(project, &args);
    ok(
        project,
        &[
            "analyze",
            "latest",
            "inconclusive",
            "--analysis",
            "measured",
        ],
    );
}

#[cfg(unix)]
#[test]
fn runs_carry_their_configuration_and_variance_pools_repeats() {
    let temp = tempdir().unwrap();
    let p = temp.path();
    fs::create_dir(p.join(".isuscope")).unwrap();
    let deployed = p.join("deployed");
    fs::write(&deployed, r#"{"commit":"abc","workers":4}"#).unwrap();
    fs::write(
        p.join(".isuscope/config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '{{\"type\":\"isuscope.result\",\"score\":%s,\"pass\":true}}\n' $(cat {score})"]
identity_command = ["cat", "{deployed}"]
"#,
            score = p.join("score").display(),
            deployed = deployed.display(),
        ),
    )
    .unwrap();
    measure(p, 100, None);
    measure(p, 110, Some("check the variance"));
    measure(p, 90, Some("once more"));
    fs::write(&deployed, r#"{"commit":"def","workers":4}"#).unwrap();
    measure(p, 200, None);

    let list = ok(p, &["list"]);
    let rows = list["runs"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    for absent in ["id", "mode", "note", "tags"] {
        assert!(rows[0].get(absent).is_none(), "{absent}: {}", rows[0]);
    }
    let key = |row: &Value| row["config_key"].as_str().unwrap().to_owned();
    assert_eq!(key(&rows[3]).len(), 12);
    assert_eq!(key(&rows[3]), key(&rows[2]));
    assert_eq!(key(&rows[3]), key(&rows[1]));
    assert_ne!(key(&rows[0]), key(&rows[1]));
    assert_eq!(rows[2]["repeat_reason"], "check the variance");
    assert_eq!(rows[3]["repeat_reason"], Value::Null);
    assert_eq!(
        rows[0]["identity"],
        serde_json::json!({"commit": "def", "workers": "4"})
    );

    let indexed = ok(
        p,
        &[
            "sql",
            "SELECT r.config_key, r.repeat_base_run_id, r.repeat_reason, r.benchmark_started,
                    (SELECT COUNT(*) FROM run_identity i WHERE i.run_id=r.id) fields
             FROM runs r ORDER BY r.started_at",
        ],
    );
    let indexed = indexed["rows"].as_array().unwrap();
    assert_eq!(indexed[0]["repeat_base_run_id"], Value::Null);
    assert!(indexed[1]["repeat_base_run_id"].is_string());
    assert_eq!(indexed[2]["repeat_reason"], "once more");
    assert!(indexed.iter().all(|row| row["benchmark_started"] == 1));
    assert!(indexed.iter().all(|row| row["fields"] == 2));
    assert_eq!(
        indexed[0]["config_key"].as_str().unwrap().len(),
        64,
        "full key in SQL"
    );

    let variance = ok(p, &["variance", "--json"]);
    let lines = variance["runs"]["rows"].as_array().unwrap();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["spread"], Value::Null);
    assert_eq!(lines[1]["repeat"], true);
    assert_eq!(lines[1]["spread"], Value::Null);
    // Known when the third run started: 100 and 110 of one configuration.
    let mean = 105.0;
    let expected = ((100.0f64 / mean - 1.0).powi(2) + (110.0f64 / mean - 1.0).powi(2)).sqrt();
    assert!((lines[2]["spread"].as_f64().unwrap() - expected).abs() < 0.001);
    assert_eq!(lines[2]["spread_configs"], 1);
    assert_eq!(lines[2]["spread_runs"], 2);
    assert_eq!(lines[3]["spread_runs"], 3);

    let first = rows[3]["run"].as_str().unwrap();
    let one = ok(p, &["variance", "--run", first, "--json"]);
    assert_eq!(one["run"]["run"], first);
    assert_eq!(one["config_runs"].as_array().unwrap().len(), 3);
    let text = isuscope(p, &["variance"]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("spread"));

    // An index from before these columns is filled from the saved runs.
    let db = Connection::open(p.join(".isuscope/isuscope.sqlite3")).unwrap();
    db.execute_batch(
        "UPDATE runs SET config_key=NULL, repeat_reason=NULL, benchmark_started=0;
         DELETE FROM run_identity; PRAGMA user_version=9;",
    )
    .unwrap();
    drop(db);
    let restored = ok(p, &["list"]);
    assert_eq!(restored["runs"]["rows"], list["runs"]["rows"]);
}
