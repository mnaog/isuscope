use super::*;
use serde_json::Value;

/// どの出力も同じ約束で書く: runは`run`・`base`の短縮ID、表は`total_count`と`truncated`を持ち、
/// `warnings`は常にある。AIが出力をまたいで突き合わせても名前と形が変わらないことを確かめる。
fn project() -> tempfile::TempDir {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "sleep 2; printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":100,\"pass\":true}'"]

[[collectors]]
name = "host-sampler"
phase = "during"
transport = "local"
command = ["sh", "-c", """
sleep 1
printf '{"type":"metric","name":"host.cpu_busy_percent","value":40,"unit":"percent","labels":{"node":"app1","role":"web"},"timestamp":%s}\n' "$(date +%s)"
printf '{"type":"metric","name":"host.cpu_busy_percent","value":10,"unit":"percent","labels":{"node":"app2","role":"db"},"timestamp":%s}\n' "$(date +%s)"
printf '%s\n' '{"type":"metric","name":"host.total_only","value":3,"unit":"count","labels":{"node":"app1"}}'
sleep 30
"""]
"#,
    )
    .unwrap();
    project
}

fn cli(project: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(args)
        .current_dir(project)
        .output()
        .unwrap()
}

fn raw(project: &std::path::Path, args: &[&str]) -> Value {
    let output = cli(project, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn error(project: &std::path::Path, args: &[&str]) -> String {
    let output = cli(project, args);
    assert!(!output.status.success(), "{args:?} should fail");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// 表（`columns`と`rows`を持つもの）はどこでも`total_count`と`truncated`を持つ。
fn assert_tables_are_counted(value: &Value, path: &str) {
    match value {
        Value::Object(fields) => {
            if fields.contains_key("columns") && fields.contains_key("rows") {
                assert!(
                    fields.contains_key("total_count") && fields.contains_key("truncated"),
                    "table at {path} lacks total_count/truncated: {value}"
                );
            }
            for (name, field) in fields {
                assert!(
                    !["run_id", "short_id", "base_run_id", "candidate_run_id"]
                        .contains(&name.as_str()),
                    "{path}.{name} should be `run` or `base`"
                );
                assert_tables_are_counted(field, &format!("{path}.{name}"));
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| assert_tables_are_counted(item, path)),
        _ => {}
    }
}

#[test]
fn every_output_follows_the_same_contract() {
    let project = project();
    let p = project.path();
    assert!(cli(p, &["run", "--hypothesis", "base"]).status.success());
    let base = raw(p, &["list"])["runs"]["rows"][0][0]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        cli(
            p,
            &[
                "analyze",
                &base,
                "supported",
                "--analysis",
                "recorded",
                "--change",
                "c1",
                "--decision",
                "accepted"
            ]
        )
        .status
        .success()
    );
    assert!(cli(p, &["run", "--hypothesis", "next"]).status.success());
    let list = raw(p, &["list"]);
    let run = list["runs"]["rows"][0][0].as_str().unwrap().to_owned();
    assert_eq!(list["runs"]["columns"][0], "run");
    assert_eq!(run.len(), 8);
    assert!(
        cli(
            p,
            &[
                "analyze",
                &run,
                "inconclusive",
                "--base",
                &base,
                "--analysis",
                "compared"
            ]
        )
        .status
        .success()
    );

    let outputs = [
        raw(p, &["list"]),
        raw(p, &["brief", &run]),
        raw(p, &["query", &run, "--metric", "host.cpu_busy_percent"]),
        raw(p, &["query", &run, "--base", &base, "--view", "http"]),
        raw(p, &["series", &run, "--metric", "host.cpu_busy_percent"]),
        raw(p, &["sql", "SELECT id FROM runs"]),
        raw(p, &["change", "list"]),
        raw(p, &["change", "show", "c1"]),
    ];
    for output in &outputs {
        assert_eq!(output["schema_version"], 2, "{output}");
        assert!(output["warnings"].is_array(), "{output}");
        assert_tables_are_counted(output, "");
        if let Some(value) = output.get("run") {
            assert_eq!(value, &Value::from(run.as_str()), "{output}");
        }
    }
    let brief = &outputs[1];
    assert_eq!(brief["review"]["comparison"]["base"], base.as_str());
    assert!(brief["review"]["comparison"].get("score_delta").is_some());
    assert_eq!(brief["summary"]["commit_hash"], Value::Null);
    assert!(brief["next"].is_array());
    assert_eq!(outputs[3]["base"], base.as_str());
    assert!(outputs[4]["window"].is_string(), "{}", outputs[4]);

    // seriesはqueryと同じ絞り込みを受け付ける。
    let series = parsed(
        &cli(
            p,
            &[
                "series",
                &run,
                "--metric-prefix",
                "host.cpu",
                "--label-contains",
                "role=we",
            ],
        )
        .stdout,
    )
    .unwrap();
    let rows = series["rows"].as_array().unwrap();
    assert!(!rows.is_empty(), "{series}");
    assert!(rows.iter().all(|row| row["node"] == "app1"), "{series}");

    // 空の結果は理由と次の一手を添える。
    let totals = raw(p, &["series", &run, "--metric", "host.total_only"]);
    assert!(
        totals["warnings"][0]
            .as_str()
            .unwrap()
            .contains(&format!("isuscope query {run} --metric host.total_only")),
        "{totals}"
    );
    let missing = raw(p, &["query", &run, "--metric", "no.such.metric"]);
    assert!(
        missing["warnings"][0]
            .as_str()
            .unwrap()
            .contains("SELECT DISTINCT name FROM metrics"),
        "{missing}"
    );

    // errorはどのrunか、どうすればよいかを言う。
    assert!(error(p, &["brief", "nosuch"]).contains("isuscope list"));
    let window = error(p, &["series", &run, "--window", "load"]);
    assert!(
        window.contains(&run) && window.contains("--window whole"),
        "{window}"
    );
    let change = error(p, &["change", "show", "nosuch"]);
    assert!(
        change.contains("isuscope change list") && !change.contains("os error"),
        "{change}"
    );
}
