use super::*;

/// アプリ計測（app_metrics.rsが1秒ごとに書くmetric行）は、標準のmetric行として読まれ、briefの
/// `app_calls`・`app_locks`欄に負荷区間の合計が出る。initializeの間の値は入らない。
#[test]
fn app_metrics_reach_brief_summed_over_the_load_window() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", """
printf '%s\n' '{"type":"isuscope.event","name":"initialize-started"}'
sleep 1
printf '%s\n' '{"type":"isuscope.event","name":"initialize-finished"}'
sleep 3
printf '%s\n' '{"type":"isuscope.result","score":1,"pass":true}'
"""]

# app-metrics-deltaがnodeから運ぶ行と同じ形。
[[collectors]]
name = "app-metrics-delta"
phase = "after"
transport = "local"
command = ["sh", "-c", """
at() { awk -v origin='{load_started_at}' -v offset="$1" 'BEGIN { printf "%.3f", origin + offset }'; }
emit() { printf '{"type":"metric","name":"%s","value":%s,"unit":"x","labels":{%s},"timestamp":%s}\n' "$1" "$2" "$3" "$4"; }
failed='"target":"payment","outcome":"502"'
ok='"target":"payment","outcome":"204"'
lock='"lock":"store"'
emit app.call.count 50 "$failed" "$(at -0.5)"
emit app.call.count 7 "$failed" "$(at 1)"
emit app.call.duration_sum 70 "$failed" "$(at 1)"
emit app.call.duration_max 30 "$failed" "$(at 1)"
emit app.call.count 7 "$failed" "$(at 2)"
emit app.call.duration_sum 140 "$failed" "$(at 2)"
emit app.call.duration_max 50 "$failed" "$(at 2)"
emit app.call.count 6 "$ok" "$(at 2)"
emit app.call.duration_sum 30 "$ok" "$(at 2)"
emit app.lock.count 1000 "$lock" "$(at 1)"
emit app.lock.wait_sum 400 "$lock" "$(at 1)"
emit app.lock.wait_max 12 "$lock" "$(at 1)"
emit app.lock.hold_sum 900 "$lock" "$(at 1)"
emit app.lock.hold_max 3 "$lock" "$(at 1)"
"""]
"#,
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
        parsed(&output.stdout).unwrap_or_default()
    };
    isuscope(&["run", "--hypothesis", "app metrics"]);

    let brief = isuscope(&["brief", "latest"]);
    let calls = &brief["app_calls"];
    assert_eq!(calls["window"], "load", "{calls}");
    let failed = calls["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["outcome"] == "502")
        .unwrap();
    assert_eq!(failed["count"], 14.0, "{calls}");
    assert_eq!(failed["share_percent"], 70.0, "{calls}");
    assert_eq!(failed["total_ms"], 210.0, "{calls}");
    assert_eq!(failed["max_ms"], 50.0, "{calls}");
    let locks = &brief["app_locks"];
    let store = &locks["rows"][0];
    assert_eq!(store["lock"], "store", "{locks}");
    assert_eq!(store["wait_avg_ms"], 0.4, "{locks}");

    // 1秒ごとの値は、負荷区間で足して比べられる。
    let query = isuscope(&[
        "query",
        "latest",
        "--metric",
        "app.call.count",
        "--label",
        "outcome=502",
        "--scope",
        "series",
        "--window",
        "load",
    ]);
    assert_eq!(query["rows"][0]["value"], 14.0, "{query}");
}
