use super::*;

#[test]
fn enrich_replaces_parser_metrics_and_survives_reindexing() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let write_config = |value: i64| {
        fs::write(
            config_dir.join("config.toml"),
            format!(
                r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf 'viewer completed: 10\nscore: 123\n'"]
score_pattern = "score: ([0-9]+)"

[[benchmark.parsers]]
name = "contest-output"
command = ["sh", "-c", "printf '%s\\n' '{{\"type\":\"metric\",\"name\":\"benchmark.viewer.completed\",\"value\":{value},\"unit\":\"viewers\",\"timestamp\":\"2026-08-27T12:34:56Z\"}}'"]
"#,
            ),
        )
        .unwrap();
    };
    write_config(10);

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "parser emits a queryable viewer metric",
            "--note",
            "initial parser",
            "--tag",
            "candidate",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (note, value, labels): (String, f64, String) = database
        .query_row(
            "SELECT r.note, m.value, m.labels_json FROM runs r JOIN metrics m ON m.run_id=r.id WHERE m.name='benchmark.viewer.completed'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(note, "initial parser");
    assert_eq!(value, 10.0);
    assert!(labels.contains("isuscope.parser"));
    assert_eq!(
        database
            .query_row(
                "SELECT COUNT(*) FROM run_tags WHERE tag='candidate'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    drop(database);

    write_config(20);
    let enrich = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["enrich", "candidate"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        enrich.status.success(),
        "{}",
        String::from_utf8_lossy(&enrich.stderr)
    );
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (count, value): (i64, f64) = database
        .query_row(
            "SELECT COUNT(*), MAX(value) FROM metrics WHERE name='benchmark.viewer.completed'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(value, 20.0);
    assert_eq!(
        database
            .query_row(
                "SELECT observed_at FROM metrics WHERE name='benchmark.viewer.completed'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "2026-08-27T12:34:56+00:00"
    );
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM enrichment_runs", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(database);

    let removed_annotate = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["annotate", "candidate"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!removed_annotate.status.success());
    let listed = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["list"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["runs"][0]["note"], "initial parser");
    assert_eq!(listed["runs"][0]["tags"], serde_json::json!(["candidate"]));

    for suffix in ["", "-wal", "-shm"] {
        let path = config_dir.join(format!("isuscope.sqlite3{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let restored = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["brief", "candidate"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(restored.status.success());
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        database
            .query_row(
                "SELECT value FROM metrics WHERE name='benchmark.viewer.completed'",
                [],
                |row| row.get::<_, f64>(0)
            )
            .unwrap(),
        20.0
    );
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM enrichment_runs", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

fn write_parser_config(config_dir: &std::path::Path, parser: &str) {
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf 'score: 123\n'"]
score_pattern = "score: ([0-9]+)"

[[benchmark.parsers]]
name = "contest-output"
command = ["sh", "-c", {parser:?}]
"#
        ),
    )
    .unwrap();
}

fn isuscope_in(project: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(args)
        .current_dir(project)
        .output()
        .unwrap()
}

fn viewer_metric(value: i64) -> String {
    format!(
        "printf '%s\\n' '{{\"type\":\"metric\",\"name\":\"benchmark.viewer.completed\",\"value\":{value},\"unit\":\"viewers\"}}'"
    )
}

/// enrichは最初に読んだrun.jsonを最後に書き戻していたので、その間にanalyzeが加えたanalysisを
/// 消していた（run.jsonは0件、SQLiteは1件、開き直すとpendingへ戻る）。enrichはrunを握ってから
/// 読み、analyzeはenrichが確定するまで待つ。
#[test]
fn analysis_written_while_enrich_runs_is_kept() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    write_parser_config(&config_dir, &viewer_metric(10));
    let run = isuscope_in(
        project.path(),
        &["run", "--hypothesis", "enrich and analyze"],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // parserが動いている間にanalyzeする。
    let marker = project.path().join("parser-started");
    write_parser_config(
        &config_dir,
        &format!(
            "touch '{}'; sleep 2; {}",
            marker.display(),
            viewer_metric(20)
        ),
    );
    let enrich = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["enrich", "latest"])
        .current_dir(project.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while !marker.exists() {
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let analyze = isuscope_in(
        project.path(),
        &[
            "analyze",
            "latest",
            "inconclusive",
            "--analysis",
            "enrich中の分析",
        ],
    );
    assert!(
        analyze.status.success(),
        "{}",
        String::from_utf8_lossy(&analyze.stderr)
    );
    let enrich = enrich.wait_with_output().unwrap();
    assert!(
        enrich.status.success(),
        "{}",
        String::from_utf8_lossy(&enrich.stderr)
    );

    let run_dir = fs::read_dir(config_dir.join("runs"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() != ".incomplete")
        .unwrap()
        .path();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(run_dir.join("run.json")).unwrap()).unwrap();
    assert_eq!(
        manifest["analyses"].as_array().unwrap().len(),
        1,
        "{manifest}"
    );
    assert_eq!(manifest["analysis_status"], "complete");
    assert_eq!(manifest["enrichments"][0]["status"], "complete");
    let listed: serde_json::Value =
        serde_json::from_slice(&isuscope_in(project.path(), &["list"]).stdout).unwrap();
    assert_eq!(listed["runs"][0]["analysis_status"], "complete", "{listed}");
}

/// enrichはrun.jsonを書いた時点で確定する。その後、SQLiteを確定する前に落ちたら（SQLiteは前の
/// metricと前のrun.jsonの印のまま）、次に開いたときに保存済みのrunから入れ直す。run.jsonの状態も
/// parserの結果に合わせて決め直す。
#[test]
fn enrich_interrupted_before_the_index_is_reindexed_and_settles_the_state() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    write_parser_config(&config_dir, &viewer_metric(10));
    let run = isuscope_in(
        project.path(),
        &["run", "--hypothesis", "interrupted enrich"],
    );
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let listed = |project: &std::path::Path| -> serde_json::Value {
        serde_json::from_slice(&isuscope_in(project, &["list"]).stdout).unwrap()
    };
    assert_eq!(listed(project.path())["runs"][0]["state"], "complete");

    // parserが失敗すればdegraded、直して入れ直せばcompleteに戻る。
    write_parser_config(&config_dir, "exit 3");
    let failed = isuscope_in(project.path(), &["enrich", "latest"]);
    assert!(!failed.status.success());
    assert_eq!(listed(project.path())["runs"][0]["state"], "degraded");
    write_parser_config(&config_dir, &viewer_metric(20));
    let fixed = isuscope_in(project.path(), &["enrich", "latest"]);
    assert!(
        fixed.status.success(),
        "{}",
        String::from_utf8_lossy(&fixed.stderr)
    );
    assert_eq!(listed(project.path())["runs"][0]["state"], "complete");

    // run.jsonを書いた後、SQLiteを確定する前に落ちた状態を作る。
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    database
        .execute_batch(
            "UPDATE metrics SET value=10 WHERE name='benchmark.viewer.completed';
             UPDATE runs SET manifest_stamp='written-before-this-run.json';",
        )
        .unwrap();
    drop(database);
    let brief = isuscope_in(project.path(), &["brief", "latest"]);
    assert!(
        brief.status.success(),
        "{}",
        String::from_utf8_lossy(&brief.stderr)
    );
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (count, value): (i64, f64) = database
        .query_row(
            "SELECT COUNT(*), MAX(value) FROM metrics WHERE name='benchmark.viewer.completed'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((count, value), (1, 20.0));
    // 前の世代のsnapshotは残らない。
    let run_dir = fs::read_dir(config_dir.join("runs"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() != ".incomplete")
        .unwrap()
        .path();
    let snapshots = fs::read_dir(&run_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("structured")
        })
        .count();
    assert_eq!(snapshots, 1);
}
