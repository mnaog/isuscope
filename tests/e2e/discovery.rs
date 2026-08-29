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
command = ["sh", "-c", "test \"$ISUSCOPE_BENCHMARK_PROTOCOL\" = v1; test \"$ISUSCOPE_PROJECT_ROOT\" = \"$PWD\"; test -f \"$ISUSCOPE_RUN_DIR/run.json\"; printf 'webappの初期化を行います\nベンチマーク走行前のデータ整合性チェックを行います\nスコア: 12345\n'"]

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
    let list: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list["schema_version"], 1);
    assert_eq!(list["runs"].as_array().unwrap().len(), 1);
    assert_eq!(list["runs"][0]["score"], 12345);
    assert_eq!(list["runs"][0]["analysis_status"], "pending");

    let report = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["report", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(report.status.success());
    let report: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["schema_version"], 6);
    assert_eq!(report["run"]["benchmark"]["score"], 12345);
    assert!(report.get("evidence").is_none());
    assert_eq!(report["transitions"]["items"][0]["count"], 7);

    let removed_include_evidence = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["report", "latest", "--include-evidence"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!removed_include_evidence.status.success());

    let removed_full = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["report", "latest", "--full"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!removed_full.status.success());

    let removed_format = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["report", "latest", "--format", "html"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!removed_format.status.success());
    let removed_output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["report", "latest", "--output", "report.json"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!removed_output.status.success());

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
        b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut response = String::new();
    std::io::Read::read_to_string(&mut connection, &mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("text/html"));
    assert!(response.contains("<!doctype html>"));
    assert!(response.contains("HTTP routes · total time"));
    assert!(response.contains("Coverage"));
    assert!(response.contains("Database queries · total time"));
    assert!(response.contains("CPU symbols · sample share"));
    assert!(response.contains("Host metrics"));
    assert!(response.contains("Profile artifacts"));
    assert!(response.contains("Transitions"));
    assert!(response.contains("Collectors"));
    assert!(response.contains("id=\"isuscope-report\""));

    let mut connection = std::net::TcpStream::connect("127.0.0.1:3000").unwrap();
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

    let mut connection = std::net::TcpStream::connect("127.0.0.1:3000").unwrap();
    std::io::Write::write_all(
        &mut connection,
        b"GET /diff HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut response = String::new();
    std::io::Read::read_to_string(&mut connection, &mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("form-action 'self'"));
    assert!(response.contains("<h1>Compare runs</h1>"));

    let mut connection = std::net::TcpStream::connect("127.0.0.1:3000").unwrap();
    std::io::Write::write_all(
        &mut connection,
        b"GET /diff?base=latest&candidate=latest HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut response = String::new();
    std::io::Read::read_to_string(&mut connection, &mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("<h2>CPU symbols</h2>"));
    assert!(response.contains("id=\"isuscope-diff\""));

    let mut connection = std::net::TcpStream::connect("127.0.0.1:3000").unwrap();
    std::io::Write::write_all(
        &mut connection,
        b"GET /api/diff?base=latest&candidate=latest HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut response = String::new();
    std::io::Read::read_to_string(&mut connection, &mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("application/json"));
    assert!(response.contains("\"schema_version\": 1"));
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
    let series: serde_json::Value = serde_json::from_slice(&series.stdout).unwrap();
    assert_eq!(series["schema_version"], 1);
    assert_eq!(series["mode"], "overview");
    assert_eq!(series["window"]["bucket_seconds"], 5);
    assert_eq!(series["rows"][0]["from_seconds"], 0);
    assert!(series["rows"][0]["cpu_percent_average"].is_null());

    let metrics = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["metrics", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(metrics.status.success());
    let metrics: serde_json::Value = serde_json::from_slice(&metrics.stdout).unwrap();
    assert_eq!(metrics["schema_version"], 1);
    let cpu = metrics["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|metric| metric["name"] == "cpu")
        .unwrap();
    assert_eq!(cpu["timestamped_rows"], 1);
    assert_eq!(cpu["labels"][0]["key"], "collector");
    assert_eq!(cpu["labels"][0]["examples"][0], "calculated");

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
    let filtered: serde_json::Value = serde_json::from_slice(&filtered.stdout).unwrap();
    assert_eq!(filtered["mode"], "metrics");
    assert_eq!(filtered["window"]["bucket_seconds"], 10);
    assert_eq!(filtered["filters"]["labels"][0]["key"], "collector");
    assert_eq!(filtered["filters"]["labels"][0]["value"], "calculated");
    assert_eq!(filtered["rows"][0]["metric"], "cpu");
    assert_eq!(filtered["rows"][0]["value"], 12.5);
    assert_eq!(filtered["rows"][0]["aggregation"], "average");
    assert_eq!(filtered["rows"][0]["labels"]["collector"], "calculated");
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
