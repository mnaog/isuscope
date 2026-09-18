use super::*;

#[test]
fn init_is_non_interactive_and_preserves_existing_files() {
    let project = tempdir().unwrap();
    let first = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("init")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(first.status.success());
    let config = project.path().join(".isuscope/config.toml");
    assert!(config.is_file());
    assert!(project.path().join(".isuscope/setup.sh").is_file());
    let benchmark = project.path().join(".isuscope/benchmark.sh");
    assert!(benchmark.is_file());
    let benchmark_parser = project.path().join(".isuscope/parse-benchmark.sh");
    assert!(benchmark_parser.is_file());
    assert!(
        fs::read_to_string(&config)
            .unwrap()
            .contains(".isuscope/benchmark.sh")
    );
    let generated = fs::read_to_string(&config).unwrap();
    assert!(generated.contains("# [context.agent]"));
    assert!(generated.contains("# history_dir = \"docs/agent-history\""));
    assert!(generated.contains("$log.$1"));
    assert!(generated.contains("failed fingerprint validation"));
    assert!(generated.contains("gzip -t"));
    let parsed: toml::Value = toml::from_str(&generated).unwrap();
    let collectors = parsed["collectors"].as_array().unwrap();
    for name in [
        "perf-start",
        "perf-stop",
        "perf-report",
        "perf-flamegraph",
        "perf-series",
        "offcpu",
        "nginx-log-delta",
        "alp",
        "mysql-log-delta",
        "slp",
    ] {
        let collector = collectors
            .iter()
            .find(|collector| collector["name"].as_str() == Some(name))
            .unwrap();
        let command = collector["command"].as_array().unwrap();
        let script = command[2].as_str().unwrap();
        assert!(
            Command::new("sh")
                .args(["-n", "-c", script])
                .status()
                .unwrap()
                .success(),
            "invalid shell syntax in {name}"
        );
    }
    for name in [
        "sysstat",
        "perf-start",
        "perf-stop",
        "perf-report",
        "perf-flamegraph",
        "perf-series",
        "offcpu",
        "nginx-log-mark",
        "nginx-log-delta",
        "alp",
        "mysql-log-mark",
        "mysql-log-delta",
        "slp",
    ] {
        assert!(
            collectors
                .iter()
                .any(|collector| collector["name"].as_str() == Some(name))
        );
    }
    assert!(
        fs::read_to_string(&benchmark)
            .unwrap()
            .contains("isuscope benchmark adapter protocol v1")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            fs::metadata(&benchmark).unwrap().permissions().mode() & 0o111,
            0
        );
        assert_ne!(
            fs::metadata(&benchmark_parser)
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
    fs::write(&config, "user-owned\n").unwrap();
    fs::write(&benchmark, "user-owned benchmark adapter\n").unwrap();

    let second = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("init")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(second.status.success());
    assert_eq!(fs::read_to_string(config).unwrap(), "user-owned\n");
    assert_eq!(
        fs::read_to_string(benchmark).unwrap(),
        "user-owned benchmark adapter\n"
    );
}

#[test]
fn init_prints_the_same_collectors_for_another_tool_to_render() {
    let project = tempdir().unwrap();
    // Defaults: what a newcomer gets on disk.
    let default = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["init", "--print", "config"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(default.status.success());
    let default = String::from_utf8(default.stdout).unwrap();
    assert!(
        !default.contains("@@"),
        "placeholders were left in the output"
    );
    let parsed: toml::Value = toml::from_str(&default).unwrap();
    assert_eq!(parsed["data_dir"].as_str().unwrap(), ".isuscope/data");
    assert!(parsed["ssh"]["user"].as_str().is_some());
    assert!(default.contains("/var/log/nginx/access.log"));

    // What a project that renders its own SSH and nodes asks for.
    let rendered = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args([
            "init",
            "--print",
            "config",
            "--no-scaffold",
            "--data-dir",
            "isuscope-data",
            "--nginx-access-log",
            "/var/log/nginx/isuscope-access.log",
            "--mysql-slow-log",
            "/var/log/mysql/isuscope-slow.log",
            "--service-units",
            "nginx.service mysql.service",
            "--sample-output",
            "config/benchmark-sample.log",
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(rendered.status.success());
    let rendered = String::from_utf8(rendered.stdout).unwrap();
    assert!(!rendered.contains("@@"));
    let parsed: toml::Value = toml::from_str(&rendered).unwrap();
    assert_eq!(parsed["data_dir"].as_str().unwrap(), "isuscope-data");
    assert_eq!(
        parsed["observability"]["service_units"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        parsed["benchmark"]["sample_output"].as_str().unwrap(),
        "config/benchmark-sample.log"
    );
    // No [ssh] or [[nodes]], so the caller can append its own without a duplicate table.
    assert!(parsed.get("ssh").is_none(), "{rendered}");
    assert!(parsed.get("nodes").is_none());
    assert!(rendered.contains("/var/log/nginx/isuscope-access.log"));
    assert_eq!(
        parsed["collectors"].as_array().unwrap().len(),
        toml::from_str::<toml::Value>(&default).unwrap()["collectors"]
            .as_array()
            .unwrap()
            .len()
    );
}
