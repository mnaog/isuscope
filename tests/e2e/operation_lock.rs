use super::*;

fn isuscope(project: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_isuscope"));
    command
        .current_dir(project)
        .env_remove("ISUSCOPE_LOCK_HELD");
    command
}

#[test]
fn lock_runs_commands_exclusively_and_reclaims_stale_owners() {
    let project = tempdir().unwrap();
    let lock = project.path().join(".local/operation.lock");
    let lock_arg = lock.display().to_string();

    // Nested lock calls inside the locked command run without re-locking.
    let nested = format!(
        "test \"$ISUSCOPE_LOCK_HELD\" = 1 && test -f '{lock_arg}/owner' && '{}' lock --path '{lock_arg}' -- sh -c 'exit 3'",
        env!("CARGO_BIN_EXE_isuscope")
    );
    let output = isuscope(project.path())
        .args(["lock", "--path", &lock_arg, "--", "sh", "-c", &nested])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!lock.exists());
    let timing = fs::read_to_string(project.path().join(".local/operation-timing.tsv")).unwrap();
    assert!(
        timing
            .lines()
            .any(|line| line.contains("\tsh\t") && line.ends_with("\t3"))
    );

    // A live owner blocks with exit status 75 and the command never runs.
    fs::create_dir_all(&lock).unwrap();
    fs::write(
        lock.join("owner"),
        format!("pid={}\noperation=deploy.sh\n", std::process::id()),
    )
    .unwrap();
    let blocked = isuscope(project.path())
        .args(["lock", "--path", &lock_arg, "--", "touch", "ran"])
        .output()
        .unwrap();
    assert_eq!(blocked.status.code(), Some(75));
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("operation=deploy.sh"));
    assert!(!project.path().join("ran").exists());

    // A dead owner is reclaimed.
    fs::write(lock.join("owner"), "pid=99999999\noperation=stale\n").unwrap();
    let reclaimed = isuscope(project.path())
        .args(["lock", "--path", &lock_arg, "--", "touch", "ran"])
        .output()
        .unwrap();
    assert!(reclaimed.status.success());
    assert!(project.path().join("ran").exists());
    assert!(!lock.exists());
}

#[test]
fn benchmark_runs_hold_the_configured_lock() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[lock]
path = ".local/operation.lock"

[benchmark]
mode = "command"
command = ["sh", "-c", "test \"$ISUSCOPE_LOCK_HELD\" = 1 && test -f .local/operation.lock/owner && touch benchmark-ran; printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":0,\"pass\":false}'"]
"#,
    )
    .unwrap();
    let lock = project.path().join(".local/operation.lock");
    fs::create_dir_all(&lock).unwrap();
    fs::write(
        lock.join("owner"),
        format!("pid={}\noperation=deploy.sh\n", std::process::id()),
    )
    .unwrap();
    let blocked = isuscope(project.path())
        .args(["run", "--hypothesis", "must wait for deploy"])
        .output()
        .unwrap();
    assert_eq!(blocked.status.code(), Some(75));
    assert!(!project.path().join("benchmark-ran").exists());

    fs::remove_file(lock.join("owner")).unwrap();
    fs::remove_dir(&lock).unwrap();
    let run = isuscope(project.path())
        .args(["run", "--hypothesis", "benchmark holds the lock"])
        .output()
        .unwrap();
    assert!(
        project.path().join("benchmark-ran").exists(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(!lock.exists());
}

#[test]
fn pin_and_route_suggestions_work_on_saved_runs() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(project.path())
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "test"]);
    fs::write(
        project.path().join(".gitignore"),
        "/.isuscope/runs/*/logs/\n/.isuscope/isuscope.sqlite3*\n",
    )
    .unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":0,\"pass\":false}'"]

[[collectors]]
name = "routes"
phase = "after"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"metric\",\"name\":\"http.requests\",\"value\":3,\"unit\":\"requests\",\"labels\":{\"route\":\"/users/42/profile\"}}' '{\"type\":\"metric\",\"name\":\"http.requests\",\"value\":2,\"unit\":\"requests\",\"labels\":{\"route\":\"/users/7/profile\"}}' '{\"type\":\"metric\",\"name\":\"http.requests\",\"value\":1,\"unit\":\"requests\",\"labels\":{\"route\":\"/login\"}}'"]
"#,
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    isuscope(project.path())
        .args(["run", "--hypothesis", "routes to suggest"])
        .output()
        .unwrap();

    let suggestions = project.path().join(".local/route-suggestions.toml");
    let suggest = isuscope(project.path())
        .args(["routes", "suggest", "latest", "--output"])
        .arg(&suggestions)
        .output()
        .unwrap();
    assert!(
        suggest.status.success(),
        "{}",
        String::from_utf8_lossy(&suggest.stderr)
    );
    let content = fs::read_to_string(&suggestions).unwrap();
    assert!(
        content.contains("pattern = \"^/users/[0-9]+/profile$\""),
        "{content}"
    );
    assert!(content.contains("replace = \"/users/:id/profile\""));
    assert!(!content.contains("/login\"\n"));

    let pin = isuscope(project.path())
        .args(["pin", "latest"])
        .output()
        .unwrap();
    assert!(
        pin.status.success(),
        "{}",
        String::from_utf8_lossy(&pin.stderr)
    );
    let staged = Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(project.path())
        .output()
        .unwrap();
    let staged = String::from_utf8_lossy(&staged.stdout);
    assert!(
        staged.lines().any(|path| path.contains("/logs/")),
        "{staged}"
    );
    assert!(staged.lines().any(|path| path.ends_with("/run.json")));
}
