use super::*;

#[test]
fn codex_context_is_required_snapshotted_and_indexed_when_configured() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    let history_dir = project.path().join("docs/codex-history");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&history_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[context.codex]
history_dir = "docs/codex-history"

[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-ran; printf '%s\\n' '{\"type\":\"isuscope.result\",\"pass\":true,\"score\":42}'"]
"#,
    )
    .unwrap();
    let history = r#"# Codex conversation

- Session: `session-a`

<!-- codex-event:session-a:turn-old:user -->
## [User] 2026-08-27T20:00:00+09:00

old request

<!-- codex-event:session-a:turn-old:codex -->
## [Codex] 2026-08-27T20:00:01+09:00

old reply

<!-- codex-event:session-a:turn-current:user -->
## [User] 2026-08-27T20:01:00+09:00

run this benchmark
"#;
    fs::write(history_dir.join("20260827-200000.md"), history).unwrap();

    let doctor = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("doctor")
        .env("CODEX_SESSION_ID", "session-a")
        .env("CODEX_THREAD_ID", "session-a")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(doctor.status.success());
    assert!(
        String::from_utf8_lossy(&doctor.stdout)
            .contains("Codex context resolved: docs/codex-history/20260827-200000.md#turn-current")
    );

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "context is linked automatically"])
        .env("CODEX_SESSION_ID", "session-a")
        .env("CODEX_THREAD_ID", "session-a")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(project.path().join("benchmark-ran").is_file());

    let run_dir = fs::read_dir(config_dir.join("runs"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() != ".incomplete")
        .unwrap()
        .path();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(run_dir.join("run.json")).unwrap()).unwrap();
    assert_eq!(
        manifest["codex_context"]["history_path"],
        "docs/codex-history/20260827-200000.md"
    );
    assert_eq!(manifest["codex_context"]["session_id"], "session-a");
    assert_eq!(manifest["codex_context"]["input_id"], "turn-current");
    assert_eq!(
        fs::read_to_string(run_dir.join("context/codex-history.md")).unwrap(),
        history
    );
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let indexed: (String, String, String) = database
        .query_row(
            "SELECT history_path, session_id, input_id FROM run_codex_context",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        indexed,
        (
            "docs/codex-history/20260827-200000.md".into(),
            "session-a".into(),
            "turn-current".into()
        )
    );
    let show = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    let output = String::from_utf8_lossy(&show.stdout);
    assert!(output.contains("codex file  docs/codex-history/20260827-200000.md"));
    assert!(output.contains("codex input turn-current"));
    drop(database);
    for suffix in ["", "-wal", "-shm"] {
        let path = config_dir.join(format!("isuscope.sqlite3{suffix}"));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let restored = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["show", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(restored.status.success());
    assert!(String::from_utf8_lossy(&restored.stderr).contains("reindexed"));
    let restored = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        restored
            .query_row("SELECT input_id FROM run_codex_context", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "turn-current"
    );
}

#[test]
fn codex_context_rejects_a_run_without_the_current_session() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    let history_dir = project.path().join("docs/codex-history");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&history_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[context.codex]
history_dir = "docs/codex-history"

[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-ran"]
"#,
    )
    .unwrap();
    fs::write(
        history_dir.join("20260827-200000.md"),
        "# Codex conversation\n\n- Session: `session-a`\n\n<!-- codex-event:session-a:turn-a:user -->\n",
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "this must never start"])
        .env_remove("CODEX_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run.stderr)
            .contains("CODEX_SESSION_ID/CODEX_THREAD_ID is not set")
    );
    assert!(!project.path().join("benchmark-ran").exists());

    let wrong_session = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "wrong session must never start"])
        .env("CODEX_SESSION_ID", "session-b")
        .env("CODEX_THREAD_ID", "session-b")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_eq!(wrong_session.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&wrong_session.stderr)
            .contains("belongs to current session `session-b`")
    );
    assert!(!project.path().join("benchmark-ran").exists());
}
