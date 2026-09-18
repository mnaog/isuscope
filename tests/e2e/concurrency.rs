use super::*;
use std::{
    thread::sleep,
    time::{Duration, Instant},
};

fn write_config(project: &std::path::Path, body: &str) {
    let config_dir = project.join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.toml"), body).unwrap();
}

fn wait_for(path: &std::path::Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{} never appeared",
            path.display()
        );
        sleep(Duration::from_millis(20));
    }
}

/// `[lock]`が無くても、後から始めた`run`は実行中の`run`を中断扱いで回収せず、開始を断る。
#[cfg(unix)]
#[test]
fn a_second_run_refuses_to_start_instead_of_taking_over_the_first() {
    let project = tempdir().unwrap();
    write_config(
        project.path(),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "touch first-started; while test ! -e release; do sleep 0.05; done; printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":1,\"pass\":false}'"]
"#,
    );
    let first = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "first"])
        .current_dir(project.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(&project.path().join("first-started"));

    let second = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "second"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("still in progress"), "{stderr}");

    fs::write(project.path().join("release"), "").unwrap();
    let first = first.wait_with_output().unwrap();
    // 1本目は横取りされずに最後まで進み、自分のrunとして確定する。
    assert_eq!(
        first.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let database = Connection::open(project.path().join(".isuscope/isuscope.sqlite3")).unwrap();
    let states = database
        .prepare("SELECT hypothesis, state FROM runs ORDER BY started_at")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(states, [("first".to_owned(), "failed".to_owned())]);
}

/// 落ちたrunの後始末は、ルール側のnode（ベンチ機）へSSHしない。
#[cfg(unix)]
#[test]
fn abandoned_run_cleanup_never_reaches_rule_side_nodes() {
    use std::os::unix::fs::PermissionsExt;

    let project = tempdir().unwrap();
    let tools = project.path().join("tools");
    fs::create_dir_all(&tools).unwrap();
    let ssh_log = project.path().join("ssh.log");
    let fake_ssh = tools.join("ssh");
    fs::write(
        &fake_ssh,
        "#!/bin/sh\nfor arg; do case \"$arg\" in *@*) printf '%s\\n' \"$arg\" >> \"$SSH_LOG\" ;; esac; done\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o755)).unwrap();
    write_config(
        project.path(),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-started; sleep 30"]

[disk]
paths = []

[[nodes]]
name = "app1"
host = "app1.internal"

[[nodes]]
name = "bench"
host = "bench.internal"
rule_side = true
"#,
    );
    let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());

    // isuscopeを後始末の暇なく落とし、`.incomplete`にrunを残す。
    let mut abandoned = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "abandoned"])
        .env("PATH", &path)
        .env("SSH_LOG", &ssh_log)
        .current_dir(project.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&project.path().join("benchmark-started"));
    abandoned.kill().unwrap();
    abandoned.wait().unwrap();
    let _ = Command::new("pkill")
        .args(["-f", "touch benchmark-started; sleep 30"])
        .status();
    let _ = fs::remove_file(&ssh_log);

    write_config(
        project.path(),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":1,\"pass\":false}'"]

[disk]
paths = []

[[nodes]]
name = "app1"
host = "app1.internal"

[[nodes]]
name = "bench"
host = "bench.internal"
rule_side = true
"#,
    );
    let recovered = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "recovers the abandoned run"])
        .env("PATH", &path)
        .env("SSH_LOG", &ssh_log)
        .current_dir(project.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&recovered.stdout);
    assert!(stdout.contains("recovered"), "{stdout}");
    let targets = fs::read_to_string(&ssh_log).unwrap_or_default();
    assert!(targets.contains("app1.internal"), "{targets}");
    assert!(!targets.contains("bench.internal"), "{targets}");
}

/// localのcollectorが`isuscope`を呼ぶときは、PATH上の古い版ではなく実行中のbinaryを使う。
#[cfg(unix)]
#[test]
fn local_collectors_run_this_isuscope_rather_than_the_one_on_path() {
    use std::os::unix::fs::PermissionsExt;

    let project = tempdir().unwrap();
    let tools = project.path().join("tools");
    fs::create_dir_all(&tools).unwrap();
    let stale = tools.join("isuscope");
    fs::write(&stale, "#!/bin/sh\nexit 3\n").unwrap();
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o755)).unwrap();
    write_config(
        project.path(),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":1,\"pass\":false}'"]

[[collectors]]
name = "self"
phase = "after"
transport = "local"
command = ["isuscope", "--version"]
"#,
    );
    Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "the collector uses this binary"])
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .current_dir(project.path())
        .output()
        .unwrap();
    let database = Connection::open(project.path().join(".isuscope/isuscope.sqlite3")).unwrap();
    let status: String = database
        .query_row(
            "SELECT status FROM collector_runs WHERE name='self'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "complete");
}
