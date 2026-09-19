use super::*;

/// 5秒bucketの先頭が区間の始まりより前だと、負荷の最初のbucketが区間から丸ごと落ちていた
/// （HTTP 100件が消え、briefのclientのpeakが10から1になった）。collectorは負荷の始まりから
/// 区切るので、最初のbucketの先頭は始まりと一致し、負荷に入る。initializeには入らない。
#[test]
fn the_first_load_bucket_is_neither_dropped_nor_counted_as_initialize() {
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
sleep 6
printf '%s\n' '{"type":"isuscope.event","name":"initialize-finished"}'
sleep 6
printf '%s\n' '{"type":"isuscope.result","score":1,"pass":true}'
"""]

# node上のalp collectorと同じく、負荷の始まりから5秒ずつ区切ったbucketを出す。
[[collectors]]
name = "alp"
phase = "after"
transport = "local"
command = ["sh", "-c", """
at() { awk -v origin='{load_started_at}' -v offset="$1" 'BEGIN { printf "%.6f", origin + offset }'; }
emit() { printf '{"type":"metric","name":"%s","value":%s,"unit":"x","labels":{%s},"timestamp":%s}\n' "$1" "$2" "$3" "$4"; }
route='"node":"app1","method":"GET","route":"/a"'
# ベンチの始まりをまたぐbucketは、始まりを先頭にする（alpのawkと同じ）。
emit http.requests 3 "$route" '{benchmark_started_at}'
emit http.requests 7 "$route" "$(at -5)"
emit http.requests 100 "$route" "$(at 0)"
emit http.requests 1 "$route" "$(at 5)"
emit client.connections_in_use 10 '"node":"app1"' "$(at 0)"
emit client.connections_in_use 1 '"node":"app1"' "$(at 5)"
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
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap_or_default()
    };
    isuscope(&["run", "--hypothesis", "bucket edges"]);
    let requests = |series: &serde_json::Value| {
        series["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row["http_requests"].as_f64())
            .sum::<f64>()
    };

    let load = isuscope(&["series", "latest", "--window", "load"]);
    assert_eq!(requests(&load), 101.0, "{load}");
    assert_eq!(load["window"]["edges"], "exact");
    let initialize = isuscope(&["series", "latest", "--window", "initialize"]);
    assert_eq!(requests(&initialize), 7.0, "{initialize}");
    // initializeの始まりはbucketの区切りでもnode上の境界でもない。
    assert_eq!(initialize["window"]["edges"], "approximate");

    // ベンチの始まりのbucketはwholeに入り、端はnode上の境界なので正確。
    let whole = isuscope(&["series", "latest", "--window", "whole"]);
    assert_eq!(requests(&whole), 111.0, "{whole}");
    assert_eq!(whole["window"]["edges"], "exact");

    let brief = isuscope(&["brief", "latest"]);
    assert_eq!(brief["hosts_window"], "load");
    assert_eq!(
        brief["clients"][0]["connections_in_use_peak"], 10.0,
        "{}",
        brief["clients"]
    );
}
