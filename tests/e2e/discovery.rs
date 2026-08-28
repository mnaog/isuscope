use super::*;

#[test]
fn discovery_run_persists_score_metrics_transitions_and_logs() {
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
command = ["sh", "-c", "test \"$ISUSCOPE_BENCHMARK_PROTOCOL\" = v1; test \"$ISUSCOPE_PROJECT_ROOT\" = \"$PWD\"; test -f \"$ISUSCOPE_RUN_DIR/run.json\"; printf 'webappの初期化を行います\nベンチマーク走行前のデータ整合性チェックを行います\nスコア: 12345\n'"]

[[collectors]]
name = "calculated"
phase = "after"
transport = "local"
modes = ["discovery-run"]
command = ["sh", "-c", "grep -q '\"started_at\":' '{run_dir}/run.json'; printf '%s\\n' '{\"type\":\"metric\",\"name\":\"cpu\",\"value\":12.5,\"unit\":\"percent\",\"timestamp\":\"2026-08-27T12:34:56.789Z\"}' '{\"type\":\"fingerprint\",\"name\":\"app.binary.sha256\",\"value\":\"abc123\"}' '{\"type\":\"transition\",\"from\":\"GET /a\",\"to\":\"GET /b\",\"count\":7}'"]
"#,
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "discovery-run",
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

    let show = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(show.status.success());
    let show_stdout = String::from_utf8(show.stdout).unwrap();
    assert!(show_stdout.contains("score       12345"));
    assert!(show_stdout.contains("transitions 1"));
    assert!(show_stdout.contains("fingerprints 1"));
    assert!(show_stdout.contains("observability"));
    assert!(show_stdout.contains("complete     calculated"));
    assert!(show_stdout.contains("metric series"));
    assert!(show_stdout.contains("cpu"));
    assert!(show_stdout.contains("sqlite      "));
    assert!(show_stdout.contains("(shared by all runs)"));
    assert!(show_stdout.contains("sql hint    sqlite3 "));
    assert!(show_stdout.contains("FROM metrics WHERE run_id="));
    assert!(show_stdout.contains("compare     sqlite3 "));
    assert!(show_stdout.contains("view    zstd -dc -- "));

    let report = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["report", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(report.status.success());
    let report: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["run"]["benchmark"]["score"], 12345);
    assert!(report.get("full").is_none());
    assert_eq!(report["transitions"]["items"][0]["count"], 7);

    let full = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["report", "latest", "--full"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(full.status.success());
    let full: serde_json::Value = serde_json::from_slice(&full.stdout).unwrap();
    assert_eq!(full["full"]["series_metrics"][0]["name"], "cpu");

    let html_path = project.path().join("report.html");
    let html = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "report",
            "latest",
            "--format",
            "html",
            "--output",
            html_path.to_str().unwrap(),
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(html.status.success());
    let html = fs::read_to_string(html_path).unwrap();
    assert!(html.contains("<!doctype html>"));
    assert!(html.contains("HTTP routes · total time"));
    assert!(html.contains("Collectors"));
    assert!(html.contains("id=\"isuscope-report\""));

    let mut ui = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("ui")
        .current_dir(project.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut connection = (0..50)
        .find_map(|_| {
            std::net::TcpStream::connect("127.0.0.1:3000")
                .ok()
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    None
                })
        })
        .expect("UI did not listen on localhost:3000");
    std::io::Write::write_all(
        &mut connection,
        b"GET /api/report HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut response = String::new();
    std::io::Read::read_to_string(&mut connection, &mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("application/json"));
    assert!(response.contains("\"score\": 12345"));
    ui.kill().unwrap();
    ui.wait().unwrap();

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
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(run_dir.join("run.json")).unwrap()).unwrap();
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
    let series_stdout = String::from_utf8(series.stdout).unwrap();
    assert!(series_stdout.contains("bucket 5s"));
    assert!(series_stdout.contains("CPU A/M%"));
    assert!(series_stdout.contains("0-"));

    let metrics = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["metrics", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(metrics.status.success());
    let metrics_stdout = String::from_utf8(metrics.stdout).unwrap();
    assert!(metrics_stdout.contains("NAME"));
    assert!(metrics_stdout.contains("cpu"));
    assert!(metrics_stdout.contains("label collector"));
    assert!(metrics_stdout.contains("calculated"));

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
    let filtered_stdout = String::from_utf8(filtered.stdout).unwrap();
    assert!(filtered_stdout.contains("bucket 10s"));
    assert!(filtered_stdout.contains("cpu"));
    assert!(filtered_stdout.contains("12.500"));
    assert!(filtered_stdout.contains("average"));
    assert!(filtered_stdout.contains("collector=calculated"));
    assert!(run_dir.join("tooling/config.toml").is_file());
    assert!(run_dir.join("tooling/isuscope-version.txt").is_file());
    assert_eq!(manifest["schema_version"], 6);
    assert_eq!(
        manifest["hypothesis"],
        "collectors preserve benchmark evidence"
    );
    assert_eq!(manifest["analysis_status"], "pending");
    assert_eq!(manifest["tooling"]["isuscope_version"], "0.8.0");
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
    let restored_show = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        restored_show.status.success(),
        "{}",
        String::from_utf8_lossy(&restored_show.stderr)
    );
    assert!(String::from_utf8_lossy(&restored_show.stderr).contains("reindexed"));
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
