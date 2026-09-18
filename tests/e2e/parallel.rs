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
