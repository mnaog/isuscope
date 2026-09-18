use super::*;
use std::{thread::sleep, time::Duration, time::Instant};

fn isuscope(project: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_isuscope"));
    command
        .current_dir(project)
        .env_remove("ISUSCOPE_LOCK_HELD");
    command
}

/// Spawns a locked command that holds the lock until `marker` is deleted.
fn spawn_holder(project: &std::path::Path, lock_arg: &str) -> std::process::Child {
    let holder = isuscope(project)
        .args([
            "lock",
            "--path",
            lock_arg,
            "--",
            "sh",
            "-c",
            "touch holding; while test -e holding; do sleep 0.05; done",
        ])
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !project.join("holding").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "holder never started"
        );
        sleep(Duration::from_millis(20));
    }
    holder
}

fn release_holder(project: &std::path::Path, mut holder: std::process::Child) {
    fs::remove_file(project.join("holding")).unwrap();
    holder.wait().unwrap();
}

#[test]
fn lock_runs_commands_exclusively_and_survives_a_crashed_holder() {
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
    // The lock file stays; only the explanation of who holds it is removed on release.
    assert!(!lock.join("owner").exists());
    let timing = fs::read_to_string(project.path().join(".local/operation-timing.tsv")).unwrap();
    assert!(
        timing
            .lines()
            .any(|line| line.contains("\tsh\t") && line.ends_with("\t3"))
    );

    // A live holder blocks with exit status 75 and the command never runs.
    let holder = spawn_holder(project.path(), &lock_arg);
    let blocked = isuscope(project.path())
        .args(["lock", "--path", &lock_arg, "--", "touch", "ran"])
        .output()
        .unwrap();
    assert_eq!(blocked.status.code(), Some(75));
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("operation=sh"));
    assert!(!project.path().join("ran").exists());
    release_holder(project.path(), holder);

    // A holder that died without cleaning up leaves an owner file, but not a lock.
    fs::write(
        lock.join("owner"),
        format!("pid={}\noperation=crashed\n", std::process::id()),
    )
    .unwrap();
    let reclaimed = isuscope(project.path())
        .args(["lock", "--path", &lock_arg, "--", "touch", "ran"])
        .output()
        .unwrap();
    assert!(
        reclaimed.status.success(),
        "{}",
        String::from_utf8_lossy(&reclaimed.stderr)
    );
    assert!(project.path().join("ran").exists());
    assert!(!lock.join("owner").exists());
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
    let lock_arg = lock.display().to_string();

    let holder = spawn_holder(project.path(), &lock_arg);
    let blocked = isuscope(project.path())
        .args(["run", "--hypothesis", "must wait for deploy"])
        .output()
        .unwrap();
    assert_eq!(blocked.status.code(), Some(75));
    assert!(!project.path().join("benchmark-ran").exists());
    release_holder(project.path(), holder);

    let run = isuscope(project.path())
        .args(["run", "--hypothesis", "benchmark holds the lock"])
        .output()
        .unwrap();
    assert!(
        project.path().join("benchmark-ran").exists(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(!lock.join("owner").exists());
}
