use super::*;

fn isuscope(project: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(args)
        .current_dir(project)
        .output()
        .unwrap()
}

fn write_config(project: &std::path::Path, benchmark: &str) {
    let config_dir = project.join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
{benchmark}

[[collectors]]
name = "order"
phase = "before"
transport = "local"
command = ["sh", "-c", "printf collector >> order.log"]
"#
        ),
    )
    .unwrap();
}

fn run_row(project: &std::path::Path) -> (String, Option<bool>, Option<i64>, String) {
    let database = Connection::open(project.join(".isuscope/isuscope.sqlite3")).unwrap();
    database
        .query_row(
            "SELECT state, passed, score, analysis_status FROM runs ORDER BY started_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

/// prepare_commandはcollectorより前に1回動き、出力はrunのlogに残る。
#[test]
fn prepare_runs_before_collectors_and_keeps_its_output() {
    let project = tempdir().unwrap();
    write_config(
        project.path(),
        r#"command = ["sh", "-c", "printf bench >> order.log; echo '{\"type\":\"isuscope.result\",\"score\":7,\"pass\":true}'"]
prepare_command = ["sh", "-c", "printf prepare: >> order.log; echo restarted app"]"#,
    );
    let output = isuscope(project.path(), &["run", "--hypothesis", "prepare first"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(project.path().join("order.log")).unwrap(),
        "prepare:collectorbench"
    );
    let (state, passed, score, _) = run_row(project.path());
    assert_eq!(
        (state.as_str(), passed, score),
        ("complete", Some(true), Some(7))
    );
    let logs = fs::read_dir(project.path().join(".isuscope/runs"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != ".incomplete")
        .flat_map(|entry| fs::read_dir(entry.path().join("logs")).unwrap())
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(
        logs.iter()
            .any(|name| name == "benchmark-prepare-stdout.zst"),
        "{logs:?}"
    );
}

/// 準備に失敗したら、collectorもベンチも動かさずにnot-startedのFAILとして残す。
#[test]
fn failed_prepare_does_not_start_the_benchmark() {
    let project = tempdir().unwrap();
    write_config(
        project.path(),
        r#"command = ["sh", "-c", "printf bench >> order.log"]
prepare_command = ["sh", "-c", "echo 'app did not become healthy' >&2; exit 3"]"#,
    );
    let output = isuscope(project.path(), &["run", "--hypothesis", "prepare fails"]);
    assert!(!output.status.success());
    assert!(!project.path().join("order.log").exists());
    let (state, passed, _, _) = run_row(project.path());
    assert_eq!((state.as_str(), passed), ("failed", Some(false)));
    let list = parsed(&isuscope(project.path(), &["list"]).stdout).unwrap();
    let failure = list["runs"]["rows"][0]["failure_reason"].to_string();
    assert!(failure.contains("app did not become healthy"), "{failure}");
}

/// adapterが判定を保留したrunはpendingで残り、次のベンチを止め、rescoreで確定する。
#[test]
fn pending_result_blocks_the_next_run_until_rescored() {
    let project = tempdir().unwrap();
    // 初回は出力の形が分からないので、adapterはscoreも合否も返さず保留する。
    write_config(
        project.path(),
        r#"command = ["sh", "-c", "echo 'score: 4242'; echo 'result=PASS'; echo '{\"type\":\"isuscope.result\",\"pending\":true,\"messages\":[\"no verified sample yet\"]}'"]
rescore_command = ["sh", "-c", "zstd -dc -- \"$1\" | grep -q 'result=PASS' && echo '{\"type\":\"isuscope.result\",\"score\":4242,\"pass\":true,\"messages\":[\"score line: score: 4242\"]}'", "rescore", "{benchmark_stdout}"]"#,
    );
    let output = isuscope(
        project.path(),
        &["survey-run", "--hypothesis", "first look"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("result    PENDING"), "{stdout}");
    assert!(stdout.contains("isuscope rescore"), "{stdout}");
    let (state, passed, score, analysis) = run_row(project.path());
    assert_eq!(
        (state.as_str(), passed, score, analysis.as_str()),
        ("pending", None, None, "not_required")
    );

    let blocked = isuscope(project.path(), &["run", "--hypothesis", "next"]);
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("pending result"));

    let rescored = isuscope(project.path(), &["rescore", "latest"]);
    assert!(
        rescored.status.success(),
        "{}",
        String::from_utf8_lossy(&rescored.stderr)
    );
    let text = String::from_utf8_lossy(&rescored.stdout);
    assert!(
        text.contains("result    PASS") && text.contains("score     4242"),
        "{text}"
    );
    let (state, passed, score, analysis) = run_row(project.path());
    assert_eq!(
        (state.as_str(), passed, score, analysis.as_str()),
        ("complete", Some(true), Some(4242), "pending")
    );
    let brief = parsed(&isuscope(project.path(), &["brief", "latest"]).stdout).unwrap();
    assert_eq!(brief["summary"]["score"], 4242);

    // 判定を記録したrunは、もう書き換えない。
    let again = isuscope(project.path(), &["rescore", "latest"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already has a benchmark result"));
}

#[test]
fn rescore_needs_a_configured_command() {
    let project = tempdir().unwrap();
    write_config(
        project.path(),
        r#"command = ["sh", "-c", "echo '{\"type\":\"isuscope.result\",\"pending\":true}'"]"#,
    );
    isuscope(project.path(), &["run", "--hypothesis", "pending"]);
    let output = isuscope(project.path(), &["rescore", "latest"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("rescore_command is not configured"));
}
