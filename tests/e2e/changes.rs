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
    serde_json::from_str(&ok(project, args)).unwrap()
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
        ],
    );

    ok(
        p,
        &[
            "change",
            "create",
            "items",
            "--description",
            "<script>items</script>",
            "--target",
            "existing-row update only",
        ],
    );
    assert!(
        !cli(
            p,
            &["change", "create", "../escape", "--description", "bad"]
        )
        .status
        .success()
    );
    assert!(
        !cli(
            p,
            &["change", "create", "items", "--description", "overwrite"]
        )
        .status
        .success()
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
    assert_eq!(brief["review"]["latest_analysis"]["base_run_id"], base);
    assert_eq!(brief["review"]["comparison"]["score"]["delta"], -10);
    assert_eq!(
        brief["review"]["changes"][0]["latest_decision"]["status"],
        "accepted"
    );
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
    assert_eq!(history["decisions"].as_array().unwrap().len(), 4);
    assert_eq!(
        history["decisions"][0]["evidence"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(history["decisions"][0]["evidence"][0]["source"]["state_sha256"].is_string());

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
    assert_eq!(restored["review"]["latest_analysis"]["base_run_id"], base);
    let db = Connection::open(p.join(".isuscope/isuscope.sqlite3")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM change_decisions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM change_decision_runs", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        5
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
    db.execute(
        "UPDATE runs SET analysis_status='pending' WHERE id=?1",
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
            "change",
            "create",
            "registration",
            "--description",
            "registration lookup",
        ],
    );
    ok(
        p,
        &[
            "change",
            "decide",
            "registration",
            "deferred",
            "--run",
            &candidate,
            "--reason",
            "investigate separately",
        ],
    );
    assert_eq!(
        json(p, &["brief", &candidate])["review"]["changes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let store = isuscope::storage::Store::open(&p.join(".isuscope")).unwrap();
    let manifest = store.load(&candidate).unwrap();
    let review = store.run_review(&manifest).unwrap();
    let mut report = isuscope::report::build(manifest, vec![], vec![], p.join("logs"), None);
    report.review = Some(review);
    let mut html = Vec::new();
    isuscope::report::write_html(&report, &mut html).unwrap();
    let html = String::from_utf8(html).unwrap();
    assert!(html.contains("&lt;script&gt;items&lt;/script&gt;"));
    assert!(!html.contains("<script>items</script>"));
    assert!(html.contains("inconclusive"));
    assert!(html.contains("各1走"));
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
    // A skipped analysis takes its reason through --analysis as well as --reason.
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
        .map(|item| item["run_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(evidence.len(), 2);
    assert!(evidence[0].ends_with(&candidate) && evidence[1].ends_with(&base));

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
