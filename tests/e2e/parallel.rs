use super::*;

#[cfg(unix)]
#[test]
fn collectors_run_per_node_in_order_and_across_nodes_at_once() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    let tools = project.path().join("tools");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&tools).unwrap();
    // Records "<host> <command>" for every call, then runs the command locally.
    let fake_ssh = tools.join("ssh");
    fs::write(
        &fake_ssh,
        "#!/bin/sh\nfor last; do :; done\nfor arg; do case \"$arg\" in *@*) host=$arg ;; esac; done\nprintf '%s %s\\n' \"$host\" \"$last\" >> \"$ORDER_LOG\"\nexec /bin/sh -c \"$last\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"isuscope.result\",\"pass\":true,\"score\":1}'"]

[disk]
paths = []

[[nodes]]
name = "app1"
host = "app1.internal"

[[nodes]]
name = "app2"
host = "app2.internal"

[[collectors]]
name = "first"
phase = "before"
transport = "ssh"
command = ["sleep 1; echo first"]

[[collectors]]
name = "second"
phase = "before"
transport = "ssh"
command = ["sleep 1; echo second"]
"#,
    )
    .unwrap();
    let order_log = project.path().join("order.log");
    let started = Instant::now();
    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "collectors run per node"])
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("ORDER_LOG", &order_log)
        .current_dir(project.path())
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let logged = fs::read_to_string(&order_log).unwrap();
    for host in ["app1.internal", "app2.internal"] {
        let commands = logged
            .lines()
            .filter(|line| line.contains(host))
            .collect::<Vec<_>>();
        assert_eq!(commands.len(), 2, "{logged}");
        // The configured order holds on each node, which is what perf and the log marks need.
        assert!(commands[0].contains("echo first"), "{logged}");
        assert!(commands[1].contains("echo second"), "{logged}");
    }
    // Four one second collectors: two nodes in parallel take about two seconds, not four.
    assert!(
        elapsed.as_secs_f64() < 3.5,
        "collectors did not overlap: {elapsed:?}"
    );
}

/// The only saved run's `run.json`.
fn only_manifest(config_dir: &std::path::Path) -> serde_json::Value {
    let runs = fs::read_dir(config_dir.join("runs"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("run.json").is_file())
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 1);
    parsed(&fs::read(runs[0].path().join("run.json")).unwrap()).unwrap()
}

fn stage<'a>(manifest: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    manifest["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stage| stage["name"] == name)
        .unwrap_or_else(|| panic!("no stage {name}: {manifest}"))
}

fn stage_span(manifest: &serde_json::Value, name: &str) -> (i64, i64) {
    let stage = stage(manifest, name);
    let start = chrono::DateTime::parse_from_rfc3339(stage["started_at"].as_str().unwrap())
        .unwrap()
        .timestamp_millis();
    (start, start + stage["duration_ms"].as_i64().unwrap())
}

#[cfg(unix)]
#[test]
fn sequences_on_one_node_run_at_once_and_keep_their_own_order() {
    use std::os::unix::fs::PermissionsExt;

    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    let tools = project.path().join("tools");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&tools).unwrap();
    let fake_ssh = tools.join("ssh");
    fs::write(
        &fake_ssh,
        "#!/bin/sh\nfor last; do :; done\nexec /bin/sh -c \"$last\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let order_log = project.path().join("order.log");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{{\"type\":\"isuscope.result\",\"pass\":true,\"score\":1}}'"]

[disk]
paths = []

[[nodes]]
name = "app1"
host = "app1.internal"

[[collectors]]
name = "perf-like-start"
phase = "before"
transport = "ssh"
sequence = "perf"
command = ["sh", "-c", "sleep 1; echo start >> {log}"]

[[collectors]]
name = "perf-like-report"
phase = "before"
transport = "ssh"
sequence = "perf"
command = ["sh", "-c", "echo report >> {log}"]

[[collectors]]
name = "log-mark"
phase = "before"
transport = "ssh"
sequence = "log"
command = ["sh", "-c", "sleep 1; echo mark >> {log}"]
"#,
            log = order_log.display()
        ),
    )
    .unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "sequences run at once"])
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let logged = fs::read_to_string(&order_log).unwrap();
    let position = |line: &str| logged.lines().position(|logged| logged == line).unwrap();
    // The same sequence keeps the configured order.
    assert!(position("start") < position("report"), "{logged}");
    let manifest = only_manifest(&config_dir);
    // Two one second sequences on one node overlap: about one second, not two.
    let before = stage(&manifest, "collectors_before")["duration_ms"]
        .as_u64()
        .unwrap();
    assert!(before < 1_800, "sequences did not overlap: {before} ms");
    // Outputs stay in configured order, and each collector records how long it took.
    let collectors = manifest["collectors"].as_array().unwrap();
    let names = collectors
        .iter()
        .map(|collector| collector["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["perf-like-start", "perf-like-report", "log-mark"]);
    assert!(collectors[0]["duration_ms"].as_u64().unwrap() >= 1_000);
    assert!(collectors[0]["started_at"].is_string());
}

#[cfg(unix)]
#[test]
fn during_collectors_stop_at_once_and_parsers_overlap_the_after_phase() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    // Both ignore SIGTERM, so each one holds its stop for the whole two second grace period.
    let stubborn = "trap '' TERM; while :; do sleep 0.1; done";
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{{\"type\":\"isuscope.result\",\"pass\":true,\"score\":1}}'"]

[[benchmark.parsers]]
name = "slow-parser"
command = ["sh", "-c", "sleep 1"]

[disk]
paths = []

[[collectors]]
name = "stubborn-one"
phase = "during"
command = ["sh", "-c", "{stubborn}"]

[[collectors]]
name = "stubborn-two"
phase = "during"
command = ["sh", "-c", "{stubborn}"]

[[collectors]]
name = "slow-after"
phase = "after"
command = ["sh", "-c", "sleep 1"]
"#
        ),
    )
    .unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "stop at once"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(stdout.contains("\ntiming    "), "{stdout}");

    let manifest = only_manifest(&config_dir);
    // Two two second grace periods side by side, not one after the other.
    let stop = stage(&manifest, "collectors_during_stop")["duration_ms"]
        .as_u64()
        .unwrap();
    assert!(
        stop < 3_500,
        "during collectors stopped one by one: {stop} ms"
    );
    let names = manifest["collectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|collector| collector["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["stubborn-one", "stubborn-two", "slow-after"]);
    // The benchmark parser reads only the benchmark output, so it runs while the during
    // collectors stop and the after collectors run.
    let parsers = stage_span(&manifest, "benchmark_parsers");
    let stopping = stage_span(&manifest, "collectors_during_stop");
    assert!(
        parsers.0 < stopping.1 && stopping.0 < parsers.1,
        "parsers {parsers:?} did not overlap {stopping:?}"
    );
    assert!(manifest["enrichments"][0]["duration_ms"].as_u64().unwrap() >= 1_000);
}

#[cfg(unix)]
#[test]
fn during_collectors_stop_while_the_boundary_phase_runs() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    // Ignores SIGTERM, so stopping it takes the whole two second grace period.
    let stubborn = "trap '' TERM; while :; do sleep 0.1; done";
    fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{{\"type\":\"isuscope.result\",\"pass\":true,\"score\":1}}'"]

[disk]
paths = []

[[collectors]]
name = "stubborn"
phase = "during"
command = ["sh", "-c", "{stubborn}"]

[[collectors]]
name = "slow-boundary"
phase = "boundary"
command = ["sh", "-c", "sleep 2"]
"#
        ),
    )
    .unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "stop during the boundary"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let manifest = only_manifest(&config_dir);
    let boundary = stage_span(&manifest, "collectors_boundary");
    let stopping = stage_span(&manifest, "collectors_during_stop");
    assert!(
        stopping.0 <= boundary.0 + 100 && stopping.1 < boundary.1 + 1_500,
        "stopping {stopping:?} did not overlap the boundary phase {boundary:?}"
    );
    assert_eq!(manifest["collectors"][1]["name"], "stubborn");
}
