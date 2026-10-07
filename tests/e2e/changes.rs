use super::*;
use serde_json::Value;

fn cli(project: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(args)
        .current_dir(project)
        .output()
        .unwrap()
}

fn ok(project: &std::path::Path, args: &[&str]) -> String {
    let out = cli(project, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn json(project: &std::path::Path, args: &[&str]) -> Value {
    parsed(ok(project, args).as_bytes()).unwrap()
}

/// briefがrunを示す短縮形（`isuscope run`が表示するもの）。
fn short(id: &str) -> &str {
    &id[id.len() - 8..]
}

fn config(project: &std::path::Path, score: i64) {
    fs::write(project.join(".isuscope/config.toml"), format!(
        "[benchmark]\nmode = \"command\"\ncommand = [\"sh\", \"-c\", \"echo '{{\\\"type\\\":\\\"isuscope.result\\\",\\\"score\\\":{score},\\\"pass\\\":true}}'\"]\n"
    )).unwrap();
}

#[test]
fn decisions_are_independent_recoverable_and_concurrent() {
    let temp = tempdir().unwrap();
    let p = temp.path();
    fs::create_dir(p.join(".isuscope")).unwrap();
    config(p, 100);
    ok(p, &["run", "--hypothesis", "baseline"]);
    let base = json(p, &["list"])["runs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(
        p,
        &[
            "analyze",
            &base,
            "supported",
            "--analysis",
            "baseline recorded",
        ],
    );
    config(p, 90);
    ok(
        p,
        &[
            "run",
            "--hypothesis",
            "remove long queries and improve score",
        ],
    );
    let candidate = json(p, &["list"])["runs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // A change is created by the analysis that first decides it.
    ok(
        p,
        &[
            "analyze",
            &candidate,
            "inconclusive",
            "--base",
            &base[..12],
            "--analysis",
            "queries improved; score fell",
            "--change",
            "items",
            "--decision",
            "deferred",
            "--description",
            "<script>items</script>",
        ],
    );
    let created = json(p, &["change", "show", "items"]);
    assert_eq!(created["change"]["description"], "<script>items</script>");
    // A rejected change leaves no analysis behind.
    for args in [
        [
            "--change",
            "../escape",
            "--decision",
            "accepted",
            "--description",
            "bad",
        ],
        [
            "--change",
            "items",
            "--decision",
            "accepted",
            "--description",
            "overwrite",
        ],
    ] {
        let mut command = vec![
            "analyze",
            &candidate,
            "inconclusive",
            "--analysis",
            "rejected",
        ];
        command.extend(args);
        assert!(!cli(p, &command).status.success(), "{args:?}");
    }
    assert_eq!(
        json(p, &["brief", &candidate])["review"]["latest_analysis"]["body"],
        "queries improved; score fell"
    );
    assert!(
        !cli(
            p,
            &[
                "change",
                "decide",
                "items",
                "provisional",
                "--run",
                &candidate,
                "--reason",
                "continue"
            ]
        )
        .status
        .success()
    );
    assert!(
        !cli(
            p,
            &[
                "change", "decide", "items", "accepted", "--run", "missing", "--reason", "invalid"
            ]
        )
        .status
        .success()
    );
    ok(
        p,
        &[
            "change",
            "decide",
            "items",
            "provisional",
            "--run",
            &candidate,
            "--run",
            &base,
            "--reason",
            "keep local improvement",
            "--revisit",
            "remove registration lookup",
        ],
    );
    assert_eq!(
        json(p, &["change", "list", "--status", "provisional"])["changes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    ok(
        p,
        &[
            "change",
            "decide",
            "items",
            "accepted",
            "--run",
            &candidate,
            "--reason",
            "accept despite lower score",
        ],
    );
    let brief = json(p, &["brief", &candidate]);
    assert_eq!(
        brief["review"]["latest_analysis"]["verdict"],
        "inconclusive"
    );
    // 比較元があれば、比較に使うコマンドを示す。分析済みなので`analyze`は出さない。
    let next = brief["next"].as_array().unwrap();
    assert_eq!(next.len(), 2, "{next:?}");
    assert_eq!(
        next[0],
        format!(
            "isuscope query {} --base {} --view http --limit 20",
            short(&candidate),
            short(&base)
        )
    );
    assert_eq!(
        brief["review"]["latest_analysis"]["base_short_id"],
        short(&base)
    );
    assert_eq!(brief["review"]["comparison"]["score"]["delta"], -10);
    assert_eq!(brief["review"]["changes"][0]["status"], "accepted");
    assert!(
        json(p, &["change", "list", "--status", "provisional"])["changes"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Analysis, not adoption, controls the next benchmark.
    ok(p, &["run", "--hypothesis", "follow-up without a decision"]);
    let brief = json(p, &["brief", &candidate]);
    assert_eq!(
        brief["review"]["comparison"]["score"]["delta_percent"],
        -10.0
    );

    let mut children = Vec::new();
    for reason in ["parallel one", "parallel two"] {
        children.push(
            Command::new(env!("CARGO_BIN_EXE_isuscope"))
                .args([
                    "change", "decide", "items", "accepted", "--run", &candidate, "--reason",
                    reason,
                ])
                .current_dir(p)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let history = json(p, &["change", "show", "items"]);
    assert_eq!(history["decisions"].as_array().unwrap().len(), 5);
    assert_eq!(
        history["decisions"][0]["evidence"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // The saved record keeps each evidence run's source; the output names it in one line.
    let decision_id = history["decisions"][0]["id"].as_str().unwrap();
    let record: Value = parsed(
        &fs::read(p.join(format!(
            ".isuscope/changes/items/decisions/{decision_id}.json"
        )))
        .unwrap(),
    )
    .unwrap();
    assert!(record["evidence"][0]["source"]["state_sha256"].is_string());
    assert!(
        history["decisions"][0]["evidence"][0]
            .as_str()
            .unwrap()
            .starts_with(&candidate[candidate.len() - 8..])
    );

    // Ignore unpublished files left by an interrupted writer.
    fs::write(
        p.join(".isuscope/changes/items/decisions/.interrupted.tmp"),
        "{",
    )
    .unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let path = p.join(format!(".isuscope/isuscope.sqlite3{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    assert_eq!(json(p, &["change", "show", "items"]), history);
    let restored = json(p, &["brief", &candidate]);
    assert_eq!(
        restored["review"]["latest_analysis"]["base_short_id"],
        short(&base)
    );
    let db = Connection::open(p.join(".isuscope/isuscope.sqlite3")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM change_decisions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        5
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM change_decision_runs", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        7
    );
    assert_eq!(
        db.query_row(
            "SELECT base_run_id FROM run_analyses WHERE verdict='inconclusive'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        base
    );
    // Simulate a crash between canonical analysis publication and SQLite commit.
    db.execute("DELETE FROM run_analyses WHERE verdict='inconclusive'", [])
        .unwrap();
    // SQLiteに残るのは書き出す前のrun.jsonの印なので、書き出したrun.jsonとは一致しない。
    db.execute(
        "UPDATE runs SET analysis_status='pending', manifest_stamp=NULL WHERE id=?1",
        [&candidate],
    )
    .unwrap();
    drop(db);
    assert_eq!(
        json(p, &["list"])["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == candidate)
            .unwrap()["latest_analysis_verdict"],
        "inconclusive"
    );

    // One run can support decisions on several changes independently.
    ok(
        p,
        &[
            "analyze",
            &candidate,
            "inconclusive",
            "--base",
            &base,
            "--analysis",
            "investigate separately",
            "--change",
            "registration",
            "--decision",
            "deferred",
            "--description",
            "registration lookup",
        ],
    );
    assert_eq!(
        json(p, &["brief", &candidate])["review"]["changes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn analyze_records_a_change_decision_with_the_analysis() {
    let temp = tempdir().unwrap();
    let p = temp.path();
    fs::create_dir(p.join(".isuscope")).unwrap();
    config(p, 100);
    ok(p, &["run", "--hypothesis", "baseline"]);
    let base = json(p, &["list"])["runs"][0]["short_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // A skipped analysis takes its reason through --analysis.
    ok(
        p,
        &["analyze", &base, "skipped", "--analysis", "baseline only"],
    );
    config(p, 110);
    ok(
        p,
        &[
            "run",
            "--hypothesis",
            "shorter keepalive frees idle connections",
        ],
    );
    let candidate = json(p, &["list"])["runs"][0]["short_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // A provisional decision without a revisit condition is refused before anything is written.
    let refused = cli(
        p,
        &[
            "analyze",
            &candidate,
            "supported",
            "--analysis",
            "idle connections fell",
            "--change",
            "keepalive",
            "--decision",
            "provisional",
        ],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--revisit"));
    assert_eq!(json(p, &["list"])["runs"][0]["analysis_status"], "pending");

    let recorded = ok(
        p,
        &[
            "analyze",
            &candidate,
            "supported",
            "--base",
            &base,
            "--analysis",
            "idle connections fell and score rose",
            "--change",
            "keepalive",
            "--decision",
            "accepted",
        ],
    );
    assert!(recorded.contains("decision  accepted"), "{recorded}");
    let history = json(p, &["change", "show", "keepalive"]);
    assert_eq!(
        history["change"]["description"],
        "shorter keepalive frees idle connections"
    );
    let decision = &history["decisions"][0];
    assert_eq!(decision["status"], "accepted");
    assert_eq!(decision["reason"], "idle connections fell and score rose");
    let evidence = decision["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(evidence.len(), 2);
    assert!(evidence[0].starts_with(&candidate) && evidence[1].starts_with(short(&base)));

    // --description only names a new change.
    let duplicate = cli(
        p,
        &[
            "analyze",
            &candidate,
            "supported",
            "--analysis",
            "revised",
            "--change",
            "keepalive",
            "--decision",
            "rejected",
            "--description",
            "renamed",
        ],
    );
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already exists"));
    assert_eq!(
        json(p, &["change", "show", "keepalive"])["decisions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
