use super::*;

#[test]
fn diff_compares_resolved_runs_as_json() {
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
command = ["sh", "benchmark.sh"]
"#,
    )
    .unwrap();
    fs::write(
        project.path().join("benchmark.sh"),
        r#"score=$(cat score)
if [ "$score" = 100 ]; then duration=1000; else duration=700; fi
printf '%s\n' \
  "{\"type\":\"metric\",\"name\":\"http.requests\",\"value\":100,\"unit\":\"requests\",\"labels\":{\"node\":\"web-1\",\"method\":\"GET\",\"route\":\"/items\"}}" \
  "{\"type\":\"metric\",\"name\":\"http.request_duration_sum\",\"value\":$duration,\"unit\":\"milliseconds\",\"labels\":{\"node\":\"web-1\",\"method\":\"GET\",\"route\":\"/items\"}}" \
  "{\"type\":\"isuscope.result\",\"score\":$score,\"pass\":false}"
"#,
    )
    .unwrap();

    fs::write(project.path().join("score"), "100\n").unwrap();
    let base = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "baseline", "--tag", "diff-base"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_eq!(base.status.code(), Some(1));

    fs::write(project.path().join("score"), "120\n").unwrap();
    let candidate = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "candidate",
            "--tag",
            "diff-candidate",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_eq!(candidate.status.code(), Some(1));

    let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["diff", "diff-base", "diff-candidate"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output["schema_version"], 1);
    assert_eq!(output["base"]["tags"][0], "diff-base");
    assert_eq!(output["candidate"]["tags"][0], "diff-candidate");
    assert_eq!(output["score"]["base"], 100);
    assert_eq!(output["score"]["candidate"], 120);
    assert_eq!(output["score"]["delta"], 20);
    assert_eq!(output["score"]["delta_percent"], 20.0);
    let route = output["http"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["route"] == "/items")
        .unwrap();
    assert_eq!(route["presence"], "both");
    assert_eq!(route["total_ms"]["base"], 1000.0);
    assert_eq!(route["total_ms"]["candidate"], 700.0);
    assert_eq!(route["total_ms"]["delta"], -300.0);
    assert_eq!(route["total_ms"]["delta_percent"], -30.0);
}
