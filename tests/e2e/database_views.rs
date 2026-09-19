use super::*;

/// slp collectorの出力（fixture）をrunに通し、DBの値がseries・query・query --baseまで届くか。
/// collectorに指標を足したとき、読む側の追従漏れ（旧名しか読まない、列が無い）をここで止める。
#[test]
fn slp_output_reaches_series_query_and_comparison() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let fixture = format!(
        "{}/tests/fixtures/slp-windows-v0.2.1.out",
        env!("CARGO_MANIFEST_DIR")
    );
    // fixtureのDB全体の5秒bucketを、このrunの負荷区間の中の時刻へ移す。
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", """
printf '%s\n' '{{"type":"isuscope.event","name":"initialize-started"}}'
printf '%s\n' '{{"type":"isuscope.event","name":"initialize-finished"}}'
sleep 3
printf '%s\n' '{{"type":"isuscope.result","score":1,"pass":true}}'
"""]

[[collectors]]
name = "slp"
phase = "during"
transport = "local"
parser = "slp-windows"
command = ["sh", "-c", "sed -E \"s/\\\"timestamp\\\":[0-9]+/\\\"timestamp\\\":$(( $(date +%s) + 1 ))/\" '{fixture}'"]
"#
        ),
    )
    .unwrap();
    let isuscope = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
            .args(args)
            .current_dir(project.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap_or_default()
    };
    isuscope(&["run", "--hypothesis", "base"]);
    let first = isuscope(&["list"])["runs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    isuscope(&[
        "analyze",
        &first,
        "inconclusive",
        "--analysis",
        "比較の土台",
    ]);
    isuscope(&["run", "--hypothesis", "candidate"]);
    let runs = isuscope(&["list"]);
    let candidate = runs["runs"][0]["id"].as_str().unwrap().to_owned();
    let base = runs["runs"][1]["id"].as_str().unwrap().to_owned();

    let expected_calls = fs::read_to_string(&fixture)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|metric| metric["name"] == "db.calls")
        .map(|metric| metric["value"].as_f64().unwrap())
        .sum::<f64>();
    let series = isuscope(&["series", &candidate]);
    let calls = series["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["db_calls"].as_f64())
        .sum::<f64>();
    assert_eq!(calls, expected_calls, "{series}");
    assert!(
        series["coverage"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["collector"] == "slp"),
        "{}",
        series["coverage"]
    );

    let database = isuscope(&[
        "query", &candidate, "--view", "database", "--window", "load",
    ]);
    let row = &database["rows"][0];
    assert!(row["p99_ms"].is_number(), "{row}");
    assert!(row["max_ms"].is_number(), "{row}");

    let comparison = isuscope(&[
        "query", &candidate, "--base", &base, "--view", "database", "--window", "load",
    ]);
    let changes = &comparison["rows"][0]["changes"];
    assert!(changes["p99_ms"].is_object(), "{changes}");
    assert!(changes["max_ms"].is_object(), "{changes}");
}
