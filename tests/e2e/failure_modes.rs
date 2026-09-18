use super::*;

#[test]
fn latest_cache_failure_does_not_fail_a_saved_run() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("latest"), "blocks cache directory").unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf 'スコア: 1\n'"]
"#,
    )
    .unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "latest cache is best effort"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(run.status.success());
    assert!(
        String::from_utf8_lossy(&run.stderr)
            .contains("run was saved, but the latest log cache could not be refreshed")
    );
    assert_eq!(
        fs::read_dir(config_dir.join("runs"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != ".incomplete")
            .count(),
        1
    );
}

#[test]
fn unavailable_during_collector_is_not_misreported_as_complete() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "sleep 0.1; printf 'スコア: 1\n'"]

[[collectors]]
name = "missing-offcpu"
phase = "during"
command = ["sh", "-c", "exit 75"]
unavailable_exit_codes = [75]
"#,
    )
    .unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "missing during tooling is explicit"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(run.status.success());
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let status: String = database
        .query_row(
            "SELECT status FROM collector_runs WHERE name='missing-offcpu'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "unavailable");
}

#[test]
fn failed_initialize_is_still_finalized() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[source]
repo = "."

[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\\n' '{\"type\":\"isuscope.result\",\"pass\":false,\"score\":0,\"messages\":[\"initialize failed\"]}'; exit 1"]
"#,
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "initialize failure is retained"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));

    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (score, passed, state): (i64, bool, String) = database
        .query_row("SELECT score, passed, state FROM runs", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(score, 0);
    assert!(!passed);
    assert_eq!(state, "failed");

    let run_count = fs::read_dir(config_dir.join("runs"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != ".incomplete")
        .count();
    assert_eq!(run_count, 1);
}

#[test]
fn unavailable_optional_collector_does_not_degrade_run() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf 'スコア: 1\n'"]

[[collectors]]
name = "retired-mysql"
phase = "after"
command = ["sh", "-c", "exit 75"]
"#,
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "unavailable optional collection does not degrade the run",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(run.status.success());
    assert!(String::from_utf8_lossy(&run.stdout).contains("retired-mysql (local, unavailable)"));
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        database
            .query_row("SELECT state FROM runs", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "complete"
    );
    assert_eq!(
        database
            .query_row("SELECT status FROM collector_runs", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "unavailable"
    );
}

#[test]
fn required_collector_is_matched_by_name_and_phase() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-ran; printf 'スコア: 1\n'"]

[[collectors]]
name = "same-name"
phase = "before"
command = ["sh", "-c", "exit 1"]
required = false

[[collectors]]
name = "same-name"
phase = "after"
command = ["sh", "-c", "exit 0"]
required = true
"#,
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "required collectors are scoped to their configured phase",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();

    assert!(run.status.success());
    assert!(project.path().join("benchmark-ran").is_file());
    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    assert_eq!(
        database
            .query_row("SELECT state FROM runs", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "degraded"
    );
}

#[test]
fn nonzero_adapter_exit_cannot_report_a_passing_run() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\\n' '{\"type\":\"isuscope.result\",\"score\":999,\"pass\":true}'; exit 7"]
"#,
    )
    .unwrap();

    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "a nonzero adapter exit overrides a reported pass",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));

    let database = Connection::open(config_dir.join("isuscope.sqlite3")).unwrap();
    let (score, passed, state): (i64, bool, String) = database
        .query_row("SELECT score, passed, state FROM runs", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(score, 999);
    assert!(!passed);
    assert_eq!(state, "failed");
}

#[cfg(unix)]
#[test]
fn ssh_transport_failure_on_a_node_stops_before_the_benchmark() {
    use std::os::unix::fs::PermissionsExt;

    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    let tools = project.path().join("tools");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&tools).unwrap();
    // Records every invocation; hosts named `unreachable` fail like a rejected host key.
    let fake_ssh = tools.join("ssh");
    fs::write(
        &fake_ssh,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SSH_ARGS_LOG\"\nfor last; do :; done\ncase \"$*\" in *@unreachable*) echo 'Host key verification failed.' >&2; exit 255 ;; esac\nexec /bin/sh -c \"$last\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let config = |host: &str| {
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-ran; printf '%s\n' '{{\"type\":\"isuscope.result\",\"pass\":true,\"score\":1}}'"]

[ssh]
known_hosts_file = ".local/known-hosts"

# The fake ssh runs df on the test machine, whose / can be nearly full.
[disk]
paths = []

[[nodes]]
name = "app1"
host = "{host}"

[[collectors]]
name = "remote-before"
phase = "before"
transport = "ssh"
command = ["true"]
"#
        )
    };
    let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());
    let args_log = project.path().join("ssh-args.log");

    fs::write(config_dir.join("config.toml"), config("unreachable")).unwrap();
    let blocked = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "run",
            "--hypothesis",
            "unreachable node must not be benchmarked",
        ])
        .env("PATH", &path)
        .env("SSH_ARGS_LOG", &args_log)
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!project.path().join("benchmark-ran").exists());
    assert!(
        String::from_utf8_lossy(&blocked.stderr)
            .contains("SSH failed for every before collector on app1"),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    let logged = fs::read_to_string(&args_log).unwrap();
    // The project root may be canonicalized (for example /var -> /private/var on macOS).
    assert!(
        logged
            .split_whitespace()
            .any(|arg| arg.starts_with("UserKnownHostsFile=/")
                && arg.ends_with("/.local/known-hosts")),
        "{logged}"
    );
    assert!(logged.contains("StrictHostKeyChecking=accept-new"));
    // Collector output is text; compressing it is what keeps log deltas off the wire.
    assert!(logged.contains("Compression=yes"), "{logged}");

    fs::write(config_dir.join("config.toml"), config("reachable")).unwrap();
    let allowed = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "reachable node is benchmarked"])
        .env("PATH", &path)
        .env("SSH_ARGS_LOG", &args_log)
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        project.path().join("benchmark-ran").is_file(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
}
