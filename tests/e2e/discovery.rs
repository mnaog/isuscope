use super::*;

#[test]
fn survey_run_persists_score_metrics_transitions_and_logs() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
data_dir = ".isuscope"

[source]
repo = "."

[benchmark]
mode = "command"
command = ["sh", "-c", "test \"$ISUSCOPE_BENCHMARK_PROTOCOL\" = v1; test \"$ISUSCOPE_PROJECT_ROOT\" = \"$PWD\"; test -f \"$ISUSCOPE_RUN_DIR/run.json\"; printf 'webappの初期化を行います\nベンチマーク走行前のデータ整合性チェックを行います\nスコア: 12345\n{\"type\":\"isuscope.result\",\"pass\":true}\n'"]

[[collectors]]
name = "calculated"
phase = "after"
transport = "local"
modes = ["survey-run"]
command = ["sh", "-c", "grep -q '\"started_at\":' '{run_dir}/run.json'; printf '%s\\n' '{\"type\":\"metric\",\"name\":\"cpu\",\"value\":12.5,\"unit\":\"percent\",\"timestamp\":\"2026-08-27T12:34:56.789Z\"}' '{\"type\":\"fingerprint\",\"name\":\"app.binary.sha256\",\"value\":\"abc123\"}' '{\"type\":\"transition\",\"from\":\"GET /a\",\"to\":\"GET /b\",\"count\":7}'"]
"#,
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "survey-run",
            "--hypothesis",
            "collectors preserve benchmark evidence",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8(run.stdout).unwrap();
    assert!(stdout.contains("score     12345"));
    assert!(stdout.contains("result    PASS"));

    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (score, passed): (i64, bool) = database
        .query_row("SELECT score, passed FROM runs", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(score, 12345);
    assert!(passed);
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    let labels: String = database
        .query_row(
            "SELECT labels_json FROM metrics WHERE name='cpu'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(labels.contains("\"collector\":\"calculated\""));
    assert_eq!(
        database
            .query_row(
                "SELECT observed_at FROM metrics WHERE name='cpu'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "2026-08-27T12:34:56.789+00:00"
    );
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM transitions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM fingerprints", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM logs", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        4
    );

    let list = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("list")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(list.status.success());
    let list: serde_json::Value = parsed(&list.stdout).unwrap();
    assert_eq!(list["schema_version"], 2);
    assert_eq!(list["runs"]["rows"].as_array().unwrap().len(), 1);
    assert_eq!(list["runs"]["rows"][0]["score"], 12345);
    assert_eq!(list["runs"]["rows"][0]["analysis_status"], "pending");

    let brief = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["brief", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(brief.status.success());
    let brief: serde_json::Value = parsed(&brief.stdout).unwrap();
    assert_eq!(brief["schema_version"], 2);
    assert_eq!(brief["summary"]["score"], 12345);
    assert_eq!(brief["transitions"]["rows"][0]["count"], 7);
    // briefは並行作業する別の人も読むので、分析待ちでも`analyze`を勧めない。
    assert_eq!(brief["next"], serde_json::json!([]), "{}", brief["next"]);
    // 切っていない欄には`more`を付けない。
    assert!(brief["http"].get("more").is_none(), "{}", brief["http"]);

    // `report`, `diff` and `ui` were removed; brief, query and sql cover the same data.
    for removed in [
        vec!["report", "latest"],
        vec!["diff", "latest", "latest"],
        vec!["ui"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
            .args(&removed)
            .current_dir(project.path())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{removed:?} still exists");
    }

    let latest = config_dir.join("latest");
    assert!(latest.join("run.json").is_file());
    assert!(latest.join("logs.json").is_file());
    let readable_logs = fs::read_dir(latest.join("logs")).unwrap().count();
    assert_eq!(readable_logs, 4);
    assert!(
        fs::read_dir(latest.join("logs")).unwrap().all(|entry| entry
            .unwrap()
            .path()
            .extension()
            .unwrap()
            == "log")
    );

    let runs = config_dir.join("runs");
    let run_dir = fs::read_dir(&runs)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() != ".incomplete")
        .unwrap()
        .path();
    let manifest: serde_json::Value = parsed(&fs::read(run_dir.join("run.json")).unwrap()).unwrap();
    database
        .execute(
            "UPDATE metrics SET observed_at=?1 WHERE name='cpu'",
            [manifest["benchmark"]["started_at"].as_str().unwrap()],
        )
        .unwrap();
    let series = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["series", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(series.status.success());
    let series: serde_json::Value = parsed(&series.stdout).unwrap();
    assert_eq!(series["schema_version"], 2);
    assert_eq!(series["mode"], "overview");
    assert_eq!(series["window"], "whole");
    assert_eq!(series["range"]["bucket_seconds"], 5);
    assert_eq!(series["rows"][0]["from_seconds"], 0);
    assert!(series["rows"][0]["cpu_busy_avg_percent"].is_null());

    // What `metrics` used to print is one SQL query over the index.
    let inventory = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "sql",
            "SELECT m.name, COUNT(*) samples, SUM(m.observed_at IS NOT NULL) timestamped, \
             j.key label, j.value example \
             FROM metrics m, json_each(m.labels_json) j WHERE m.name='cpu' GROUP BY m.name, j.key",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        inventory.status.success(),
        "{}",
        String::from_utf8_lossy(&inventory.stderr)
    );
    let inventory: serde_json::Value = parsed(&inventory.stdout).unwrap();
    let cpu = &inventory["rows"][0];
    assert_eq!(cpu["timestamped"], 1);
    assert_eq!(cpu["label"], "collector");
    assert_eq!(cpu["example"], "calculated");

    let filtered = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "series",
            "latest",
            "--metric",
            "cpu",
            "--label",
            "collector=calculated",
            "--bucket",
            "10",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(filtered.status.success());
    let filtered: serde_json::Value = parsed(&filtered.stdout).unwrap();
    assert_eq!(filtered["mode"], "metrics");
    assert_eq!(filtered["range"]["bucket_seconds"], 10);
    assert_eq!(filtered["rows"][0]["metric"], "cpu");
    assert_eq!(filtered["rows"][0]["value"], 12.5);
    // 単位と集計方法はmetricごとに1回だけ出し、行では繰り返さない。
    assert_eq!(filtered["metrics"]["cpu"]["aggregation"], "average");
    assert!(filtered["rows"][0].get("aggregation").is_none());
    assert_eq!(filtered["rows"][0]["labels"]["collector"], "calculated");

    database
        .execute(
            "UPDATE metrics SET observed_at=?1 WHERE name='cpu'",
            [manifest["benchmark"]["initialize_finished_at"]
                .as_str()
                .unwrap()],
        )
        .unwrap();
    let load_query = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "query", "latest", "--scope", "series", "--window", "load", "--metric", "cpu",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        load_query.status.success(),
        "{}",
        String::from_utf8_lossy(&load_query.stderr)
    );
    let load_query: serde_json::Value = parsed(&load_query.stdout).unwrap();
    assert_eq!(load_query["window"], "load");
    assert_eq!(load_query["rows"][0]["value"], 12.5);
    assert_eq!(load_query["rows"][0]["aggregation"], "average");
    assert!(run_dir.join("tooling/config.toml").is_file());
    assert!(run_dir.join("tooling/isuscope-version.txt").is_file());
    assert_eq!(manifest["schema_version"], 6);
    assert_eq!(
        manifest["hypothesis"],
        "collectors preserve benchmark evidence"
    );
    assert_eq!(manifest["analysis_status"], "pending");
    assert_eq!(manifest["tooling"]["isuscope_version"], "0.9.0");
    assert_eq!(
        manifest["tooling"]["config_sha256"].as_str().unwrap().len(),
        64
    );

    drop(database);
    for suffix in ["", "-wal", "-shm"] {
        let path = config_dir.join(format!("isuscope.sqlite3{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let restored_list = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("list")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        restored_list.status.success(),
        "{}",
        String::from_utf8_lossy(&restored_list.stderr)
    );
    assert!(String::from_utf8_lossy(&restored_list.stderr).contains("reindexed"));
    let restored = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        restored
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        restored
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        restored
            .query_row("SELECT COUNT(*) FROM transitions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        restored
            .query_row("SELECT COUNT(*) FROM fingerprints", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
