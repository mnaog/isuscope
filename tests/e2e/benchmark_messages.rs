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
fn parser_messages_record_why_a_benchmark_failed_and_sample_its_errors() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    // Seven errors in one category keep five samples; an unknown kind is reported.
    let mut records = vec![
        r#"{"type":"message","kind":"failure","text":"validation: GET /user/0/home expected(403) != actual(401)"}"#.to_owned(),
        r#"{"type":"message","kind":"error","category":"validation","text":"validation-error-invalid-status-code"}"#.to_owned(),
        r#"{"type":"message","kind":"note","text":"not a known kind"}"#.to_owned(),
    ];
    for index in 0..7 {
        records.push(format!(
            r#"{{"type":"message","kind":"error","category":"timeout","text":"dial tcp 10.0.0.{index}:80: i/o timeout"}}"#
        ));
    }
    let parser_output = records.join("\n").replace('\'', "'\\''");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{{\"type\":\"isuscope.result\",\"pass\":false,\"score\":0}}'"]

[[benchmark.parsers]]
name = "contest-output"
command = ["sh", "-c", '''printf '%s\n' '{parser_output}' ''']
"#
        ),
    )
    .unwrap();

    let run = isuscope(
        project.path(),
        &["run", "--hypothesis", "failure is explained"],
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(!run.status.success());
    assert!(
        stdout.contains("failure   validation: GET /user/0/home expected(403) != actual(401)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("error     dial tcp 10.0.0.0:80: i/o timeout"),
        "{stdout}"
    );

    let list: Value = serde_json::from_slice(&isuscope(project.path(), &["list"]).stdout).unwrap();
    let summary = &list["runs"][0];
    let id = summary["id"].as_str().unwrap();
    assert_eq!(summary["short_id"], &id[id.len() - 8..]);
    assert_eq!(
        summary["failure"],
        "validation: GET /user/0/home expected(403) != actual(401)"
    );

    let brief = isuscope(
        project.path(),
        &["brief", summary["short_id"].as_str().unwrap()],
    );
    assert!(
        brief.status.success(),
        "{}",
        String::from_utf8_lossy(&brief.stderr)
    );
    let brief: Value = serde_json::from_slice(&brief.stdout).unwrap();
    let messages = &brief["benchmark_messages"];
    assert_eq!(
        messages["failure"][0],
        "validation: GET /user/0/home expected(403) != actual(401)"
    );
    let timeouts = messages["errors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["category"] == "timeout")
        .unwrap();
    assert_eq!(timeouts["samples"].as_array().unwrap().len(), 5);
    assert_eq!(messages["omitted_count"], 2);
    assert_eq!(brief["run"]["short_id"], summary["short_id"]);

    let manifest: Value = serde_json::from_slice(
        &fs::read(config_dir.join("runs").join(id).join("run.json")).unwrap(),
    )
    .unwrap();
    let enrichment = &manifest["enrichments"][0];
    assert!(
        enrichment["error"]
            .as_str()
            .unwrap()
            .contains("1 message records were ignored"),
        "{enrichment}"
    );
}

#[test]
fn invalid_utf8_output_is_kept_and_a_silent_failure_reports_the_benchmark_error() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    // An invalid byte sequence between lines must not end the capture or the parse.
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf 'broken \\377\\376 line\\n'; printf '%s\\n' '{\"type\":\"metric\",\"name\":\"benchmark.after_broken\",\"value\":1}' '{\"type\":\"isuscope.result\",\"pass\":true,\"score\":7}'"]
"#,
    )
    .unwrap();
    let run = isuscope(project.path(), &["run", "--hypothesis", "binary noise"]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let list: Value = serde_json::from_slice(&isuscope(project.path(), &["list"]).stdout).unwrap();
    assert_eq!(list["runs"][0]["score"], 7);
    let query = isuscope(
        project.path(),
        &["query", "latest", "--metric", "benchmark.after_broken"],
    );
    let query: Value = serde_json::from_slice(&query.stdout).unwrap();
    assert_eq!(query["total_count"], 1, "{query}");
    let id = list["runs"][0]["short_id"].as_str().unwrap().to_owned();
    isuscope(
        project.path(),
        &["analyze", &id, "skipped", "--reason", "fixture"],
    );

    fs::write(
        config_dir.join("config.toml"),
        "[benchmark]\nmode = \"command\"\ncommand = [\"sh\", \"-c\", \"exit 3\"]\n",
    )
    .unwrap();
    let failed = isuscope(project.path(), &["run", "--hypothesis", "adapter exits"]);
    assert!(!failed.status.success());
    let stdout = String::from_utf8_lossy(&failed.stdout);
    assert!(
        stdout.contains("failure   benchmark command exited with"),
        "{stdout}"
    );
    let list: Value = serde_json::from_slice(&isuscope(project.path(), &["list"]).stdout).unwrap();
    assert!(
        list["runs"][0]["failure"]
            .as_str()
            .unwrap()
            .starts_with("benchmark command exited with"),
        "{list}"
    );
}
