use super::*;
use serde_json::Value;

fn isuscope(project: &std::path::Path, args: &[&str], path: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(args)
        .current_dir(project)
        .env("PATH", path)
        .output()
        .unwrap()
}

/// 配布するcollectorは`stat -c`（GNU coreutils）を使う。BSDのstatしか無い端末では、同じ出力を
/// 返すstubを先に置く。
#[cfg(unix)]
fn path_with_gnu_stat(bin: &std::path::Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(bin).unwrap();
    let gnu = Command::new("stat")
        .args(["-c", "%s", "/"])
        .output()
        .is_ok_and(|output| output.status.success());
    if !gnu {
        let stub = bin.join("stat");
        fs::write(
            &stub,
            "#!/bin/sh\n\
             if [ \"$1\" = -c ]; then\n\
               format=$(printf '%s' \"$2\" | sed 's/%s/%z/g'); shift 2\n\
               exec /usr/bin/stat -f \"$format\" \"$@\"\n\
             fi\n\
             exec /usr/bin/stat \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    }
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
}

/// 配布する設定から状態ログのcollectorを取り出し、手元のfileを読むようにする。
#[cfg(unix)]
fn state_log_collectors(state_log: &std::path::Path) -> Vec<toml::Value> {
    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions {
        state_log: state_log.display().to_string(),
        ..Default::default()
    });
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    config["collectors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|collector| {
            collector["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("state-log-"))
        })
        .cloned()
        .collect()
}

#[cfg(unix)]
fn script(collectors: &[toml::Value], name: &str, prefix: &std::path::Path) -> String {
    collectors
        .iter()
        .find(|collector| collector["name"].as_str() == Some(name))
        .unwrap()["command"][2]
        .as_str()
        .unwrap()
        .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
}

#[cfg(unix)]
fn sh(script: &str, path: &str) -> std::process::Output {
    Command::new("sh")
        .args(["-c", script])
        .env("PATH", path)
        .output()
        .unwrap()
}

/// markとdeltaは、ベンチの前後で増えた分だけを運ぶ。作り直されたfileは先頭から、まだ無かった
/// fileはベンチの間に作られた分を読む。上限を超えた分は捨てて、その大きさを残す。
#[cfg(unix)]
#[test]
fn state_log_collectors_carry_only_the_lines_written_during_the_run() {
    let directory = tempdir().unwrap();
    let path = path_with_gnu_stat(&directory.path().join("bin"));
    let log = directory.path().join("state.jsonl");
    let prefix = directory.path().join("isuscope-t");
    let collectors = state_log_collectors(&log);
    let mark = script(&collectors, "state-log-mark", &prefix);
    let delta = script(&collectors, "state-log-delta", &prefix);
    let lines = |output: &std::process::Output| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };

    // まだ無い：markはunavailableで、ベンチの間に作られたfileをdeltaが先頭から読む。
    let output = sh(&mark, &path);
    assert_eq!(output.status.code(), Some(75), "{output:?}");
    fs::write(
        &log,
        "{\"t\":1,\"kind\":\"ride\",\"id\":\"r1\",\"from\":null,\"to\":\"A\"}\n",
    )
    .unwrap();
    let output = sh(&delta, &path);
    assert!(output.status.success(), "{output:?}");
    let first = lines(&output);
    assert_eq!(first.len(), 3, "{first:?}");
    assert!(first[0].contains("\"r1\""), "{first:?}");
    assert!(first[1].contains("state.read_bytes"), "{first:?}");
    assert!(
        first[2].contains("\"state.truncated_bytes\",\"value\":0"),
        "{first:?}"
    );

    // 続きだけを読む。
    assert!(sh(&mark, &path).status.success());
    let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
    use std::io::Write;
    writeln!(
        file,
        "{{\"t\":2,\"kind\":\"ride\",\"id\":\"r1\",\"from\":\"A\",\"to\":\"B\"}}"
    )
    .unwrap();
    drop(file);
    let second = lines(&sh(&delta, &path));
    assert_eq!(second.len(), 3, "{second:?}");
    assert!(second[0].contains("\"to\":\"B\""), "{second:?}");

    // 作り直された（小さくなった）fileは先頭から読む。
    assert!(sh(&mark, &path).status.success());
    fs::remove_file(&log).unwrap();
    fs::write(
        &log,
        "{\"t\":3,\"kind\":\"ride\",\"id\":\"r2\",\"from\":null,\"to\":\"A\"}\n",
    )
    .unwrap();
    let third = lines(&sh(&delta, &path));
    assert!(third[0].contains("\"r2\""), "{third:?}");

    // 上限を超えた分は運ばず、大きさを残す。
    assert!(sh(&mark, &path).status.success());
    fs::write(
        &log,
        "{\"t\":3,\"kind\":\"ride\",\"id\":\"r2\",\"from\":null,\"to\":\"A\"}\n0123456789\n",
    )
    .unwrap();
    let small = delta.replace("limit=268435456", "limit=4");
    let fourth = String::from_utf8(sh(&small, &path).stdout).unwrap();
    assert!(fourth.starts_with("0123\n"), "{fourth}");
    assert!(
        fourth.contains("\"state.truncated_bytes\",\"value\":7"),
        "{fourth}"
    );
}

/// 1回のrunで、状態ログが集計metric・brief・`states sql`・`states timeline`まで届く。
#[cfg(unix)]
#[test]
fn a_run_summarizes_the_state_log_and_keeps_the_raw_lines_queryable() {
    let project = tempdir().unwrap();
    let path = path_with_gnu_stat(&project.path().join("bin"));
    let log = project.path().join("state.jsonl");
    // 前のrunまでに書かれた行は運ばない。
    fs::write(
        &log,
        "{\"t\":1000,\"seq\":1,\"kind\":\"ride\",\"id\":\"old\",\"from\":null,\"to\":\"MATCHING\"}\n",
    )
    .unwrap();
    // ベンチが書く行。`@µs@`はベンチの始まりの次の秒からの経過マイクロ秒。
    let events = project.path().join("events.jsonl");
    fs::write(
        &events,
        [
            r#"{"t":@0@,"seq":1,"kind":"ride","id":"r1","from":null,"to":"MATCHING","attrs":{"user":"u1"}}"#,
            r#"{"t":@0@,"seq":2,"kind":"chair","id":"c1","from":null,"to":"IDLE"}"#,
            r#"{"t":@100000@,"seq":3,"kind":"ride","id":"r1","from":"MATCHING","to":"ENROUTE","attrs":{"chair":"c1","pickup_distance":600}}"#,
            r#"{"t":@100000@,"seq":4,"kind":"chair","id":"c1","from":"IDLE","to":"BUSY","attrs":{"ride":"r1"}}"#,
            r#"{"t":@700000@,"seq":5,"kind":"ride","id":"r1","from":"ENROUTE","to":"COMPLETED","attrs":{"chair":"c1"}}"#,
            r#"{"t":@700300@,"seq":6,"kind":"chair","id":"c1","from":"BUSY","to":"IDLE"}"#,
            r#"{"t":@700400@,"seq":7,"kind":"ride","id":"r2","from":null,"to":"MATCHING"}"#,
            r#"{"t":@800000@,"dropped":3}"#,
            "",
        ]
        .join("\n"),
    )
    .unwrap();
    let mut collectors = state_log_collectors(&log);
    for collector in &mut collectors {
        let table = collector.as_table_mut().unwrap();
        table.insert("transport".into(), "local".into());
        table.remove("roles");
    }
    let benchmark = format!(
        "base=$(( ($(date +%s) + 1) * 1000000 )); \
         awk -v base=\"$base\" '{{ while (match($0, /@[0-9]+@/)) {{ $0 = substr($0, 1, RSTART - 1) sprintf(\"%.0f\", base + substr($0, RSTART + 1, RLENGTH - 2)) substr($0, RSTART + RLENGTH) }} print }}' {events} >> {log}; \
         sleep 2.5; \
         printf '%s\\n' '{{\"type\":\"isuscope.result\",\"pass\":true,\"score\":42}}'",
        events = events.display(),
        log = log.display(),
    );
    let config = toml::toml! {
        [benchmark]
        mode = "command"
        command = ["sh", "-c", benchmark]
    };
    let mut config = toml::Value::Table(config);
    config
        .as_table_mut()
        .unwrap()
        .insert("collectors".into(), toml::Value::Array(collectors));
    fs::create_dir_all(project.path().join(".isuscope")).unwrap();
    fs::write(
        project.path().join(".isuscope/config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();

    let run = isuscope(
        project.path(),
        &["run", "--hypothesis", "state log is collected"],
        &path,
    );
    assert!(run.status.success(), "{run:?}");

    // 集計metric：r1は迎車（ENROUTE）に600ms、c1はBUSYに600.3ms。
    let brief = isuscope(project.path(), &["brief", "latest"], &path);
    let brief: Value = parsed(&brief.stdout).unwrap();
    let states = &brief["states"];
    assert_eq!(states["window"], "whole", "{states}");
    let rows = states["rows"].as_array().unwrap();
    let row = |kind: &str, state: &str| {
        rows.iter()
            .find(|row| row["kind"] == kind && row["state"] == state)
            .unwrap_or_else(|| panic!("no {kind} {state} in {states}"))
    };
    assert_eq!(row("chair", "BUSY")["total_ms"], 600.3, "{states}");
    assert_eq!(row("ride", "ENROUTE")["stays"], 1, "{states}");
    assert_eq!(row("ride", "MATCHING")["open"], 1, "{states}");
    assert_eq!(
        rows[0]["state"], "BUSY",
        "the longest total comes first: {states}"
    );
    let warnings = brief["warnings"].to_string();
    assert!(warnings.contains("dropped 3 state events"), "{warnings}");

    // 生の行：前のrunの行は入らず、このrunの7行だけ。
    let counted = isuscope(
        project.path(),
        &[
            "states",
            "sql",
            "latest",
            "SELECT kind, COUNT(*) AS events FROM state_events GROUP BY kind ORDER BY kind",
        ],
        &path,
    );
    assert!(counted.status.success(), "{counted:?}");
    let counted: Value = parsed(&counted.stdout).unwrap();
    assert_eq!(counted["rows"][0]["kind"], "chair");
    assert_eq!(counted["rows"][0]["events"], 3);
    assert_eq!(counted["rows"][1]["events"], 4);
    assert!(
        counted["warnings"].to_string().contains("dropped 3"),
        "{counted}"
    );

    // stayのviewでattrsの値ごとに分けられる。
    let stays = isuscope(
        project.path(),
        &[
            "states",
            "sql",
            "latest",
            "SELECT state, json_extract(attrs, '$.pickup_distance') AS distance, duration_ms FROM state_stays WHERE kind = 'ride' AND state = 'ENROUTE'",
        ],
        &path,
    );
    let stays: Value = parsed(&stays.stdout).unwrap();
    assert_eq!(stays["rows"][0]["distance"], 600, "{stays}");
    assert_eq!(stays["rows"][0]["duration_ms"], 600.0, "{stays}");

    // 1つの椅子の前後。ride r1はc1のattrsに、c1はr1のattrsに現れる。関係する対象の行は、
    // 椅子の最初の行の1秒前から最後の行の1秒後まで。
    let timeline = isuscope(
        project.path(),
        &[
            "states",
            "timeline",
            "latest",
            "--kind",
            "chair",
            "--id",
            "c1",
            "--related",
        ],
        &path,
    );
    assert!(timeline.status.success(), "{timeline:?}");
    let timeline: Value = parsed(&timeline.stdout).unwrap();
    let rows = timeline["rows"].as_array().unwrap();
    let relations = rows
        .iter()
        .map(|row| format!("{} {} {}", row["relation"], row["id"], row["to_state"]))
        .collect::<Vec<_>>();
    assert_eq!(
        relations,
        [
            r#""related" "r1" "MATCHING""#,
            r#""target" "c1" "IDLE""#,
            r#""related" "r1" "ENROUTE""#,
            r#""target" "c1" "BUSY""#,
            r#""related" "r1" "COMPLETED""#,
            r#""target" "c1" "IDLE""#,
        ],
        "{timeline}"
    );
    // 完了の通知から椅子が空くまで0.3ms。
    assert_eq!(rows[5]["gap_ms"], 0.3, "{timeline}");
    // r2はc1とつながらない。
    assert!(
        !relations.iter().any(|row| row.contains("r2")),
        "{timeline}"
    );

    // 無い対象は、runにある種類を示す。
    let missing = isuscope(
        project.path(),
        &[
            "states", "timeline", "latest", "--kind", "chair", "--id", "nope",
        ],
        &path,
    );
    let missing: Value = parsed(&missing.stdout).unwrap();
    assert!(
        missing["warnings"]
            .to_string()
            .contains("chair (1 objects)"),
        "{missing}"
    );

    // 一時的な索引も読み取り専用。
    let write = isuscope(
        project.path(),
        &["states", "sql", "latest", "DELETE FROM state_events"],
        &path,
    );
    assert!(!write.status.success());

    // 1秒ごとの対象の数はseriesで読める。
    let series = isuscope(
        project.path(),
        &[
            "series",
            "latest",
            "--metric",
            "state.occupancy",
            "--label",
            "state=BUSY",
            "--bucket",
            "1",
        ],
        &path,
    );
    assert!(series.status.success(), "{series:?}");
    assert!(
        String::from_utf8_lossy(&series.stdout).contains("state.occupancy"),
        "{}",
        String::from_utf8_lossy(&series.stdout)
    );
}

/// 状態ログの無いrunでも`states`は止まらず、理由を返す。
#[cfg(unix)]
#[test]
fn states_explains_a_run_without_a_state_log() {
    let project = tempdir().unwrap();
    let path = std::env::var("PATH").unwrap();
    fs::create_dir_all(project.path().join(".isuscope")).unwrap();
    fs::write(
        project.path().join(".isuscope/config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"isuscope.result\",\"pass\":true,\"score\":1}'"]
"#,
    )
    .unwrap();
    assert!(
        isuscope(
            project.path(),
            &["run", "--hypothesis", "no state log"],
            &path
        )
        .status
        .success()
    );
    let output = isuscope(
        project.path(),
        &[
            "states",
            "sql",
            "latest",
            "SELECT COUNT(*) AS events FROM state_events",
        ],
        &path,
    );
    assert!(output.status.success(), "{output:?}");
    let output: Value = parsed(&output.stdout).unwrap();
    assert_eq!(output["rows"][0]["events"], 0);
    assert!(
        output["warnings"]
            .to_string()
            .contains("no state-log collector ran"),
        "{output}"
    );
    let brief = isuscope(project.path(), &["brief", "latest"], &path);
    let brief: Value = parsed(&brief.stdout).unwrap();
    assert_eq!(brief["states"]["rows"], serde_json::json!([]), "{brief}");

    let schema = isuscope(project.path(), &["states", "sql", "--schema"], &path);
    assert!(
        String::from_utf8_lossy(&schema.stdout).contains("CREATE VIEW state_stays"),
        "{schema:?}"
    );
}
