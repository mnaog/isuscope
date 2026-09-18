use super::*;

#[cfg(unix)]
#[test]
fn signal_finalizes_aborted_run_and_executes_after_cleanup() {
    use std::{thread, time::Duration};

    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-started; printf 'webappの初期化を行います\n'; sleep 30"]

[[collectors]]
name = "cleanup"
phase = "after"
transport = "local"
command = ["sh", "-c", "printf cleaned > cleanup-ran"]
"#,
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "signal interruption is finalized safely",
        ])
        .current_dir(project.path())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if project.path().join("benchmark-started").is_file() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(project.path().join("benchmark-started").is_file());
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(child.wait().unwrap().code(), Some(1));
    assert_eq!(
        fs::read_to_string(project.path().join("cleanup-ran")).unwrap(),
        "cleaned"
    );
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (state, passed): (String, bool) = database
        .query_row("SELECT state, passed FROM runs", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(state, "aborted");
    assert!(!passed);
    assert_eq!(
        fs::read_dir(config_dir.join("runs/.incomplete"))
            .unwrap()
            .count(),
        0
    );
}

#[cfg(unix)]
#[test]
fn interruption_does_not_wait_for_a_grandchild_holding_the_pipes() {
    use std::{thread, time::Duration, time::Instant};

    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    // ベンチが孫processを残し、それがstdout/stderrを握ったまま長く生きる場合。
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "sh -c 'trap \"\" TERM; sleep 600' & touch benchmark-started; sleep 600"]
"#,
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "an interrupted run must return"])
        .current_dir(project.path())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if project.path().join("benchmark-started").is_file() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(project.path().join("benchmark-started").is_file());
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let interrupted_at = Instant::now();
    assert_eq!(child.wait().unwrap().code(), Some(1));
    assert!(
        interrupted_at.elapsed() < Duration::from_secs(20),
        "interruption took {:?}",
        interrupted_at.elapsed()
    );
}

#[test]
fn list_since_filters_runs_by_start_time() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":0,\"pass\":false}'"]
"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "a run to count"])
        .current_dir(project.path())
        .output()
        .unwrap();
    let count = |since: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
            .args(["list", "--since", since])
            .current_dir(project.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let listed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        listed["runs"].as_array().unwrap().len()
    };
    assert_eq!(count("4h"), 1);
    assert_eq!(count("2099-01-01T00:00:00+09:00"), 0);
    let invalid = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["list", "--since", "yesterday"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!invalid.status.success());
}
