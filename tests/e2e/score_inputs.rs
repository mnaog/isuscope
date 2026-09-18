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
fn score_inputs_are_collected_in_survey_run_only() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    // What the system under test says about the value the score is made of.
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"isuscope.result\",\"pass\":true,\"score\":10}'"]

[[collectors]]
name = "score-probe"
phase = "after"
transport = "local"
modes = ["survey-run"]
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"metric\",\"name\":\"score.tip_total\",\"value\":4200,\"unit\":\"points\"}'"]
"#,
    )
    .unwrap();

    assert!(
        isuscope(
            project.path(),
            &["run", "--hypothesis", "normal run skips the probe"]
        )
        .status
        .success()
    );
    let brief: Value =
        serde_json::from_slice(&isuscope(project.path(), &["brief", "latest"]).stdout).unwrap();
    assert_eq!(brief["score_inputs"]["total_count"], 0, "{brief}");
    isuscope(
        project.path(),
        &["analyze", "latest", "skipped", "--reason", "fixture"],
    );

    assert!(
        isuscope(
            project.path(),
            &[
                "survey-run",
                "--hypothesis",
                "settle what the score is made of"
            ],
        )
        .status
        .success()
    );
    let brief: Value =
        serde_json::from_slice(&isuscope(project.path(), &["brief", "latest"]).stdout).unwrap();
    let rows = brief["score_inputs"]["items"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{brief}");
    assert_eq!(rows[0]["metric"], "score.tip_total");
    assert_eq!(rows[0]["value"], 4200.0);
    // The benchmark section keeps its own metrics.
    assert_eq!(brief["benchmark"]["total_count"], 0);
}
