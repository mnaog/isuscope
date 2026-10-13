use super::*;

/// The template `mysql-digest` collector with stand-ins for `sudo` and `mysql`. The sampler's
/// cumulative snapshots are split at the last snapshot before the load start; snapshots after the
/// benchmark ended are ignored, and the output has the slp-windows shape the parser reads.
#[cfg(unix)]
#[test]
fn digest_collector_splits_cumulative_snapshots_into_initialize_and_load() {
    use std::os::unix::fs::PermissionsExt;

    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions::default());
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    let collectors = config["collectors"].as_array().unwrap();
    let command = |name: &str| {
        collectors
            .iter()
            .find(|collector| collector["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("template has no {name} collector"))["command"][2]
            .as_str()
            .unwrap()
            .to_owned()
    };
    for name in ["mysql-digest-start", "mysql-digest-sampler"] {
        command(name);
    }
    let template = command("mysql-digest");

    let directory = tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for (name, body) in [
        ("sudo", "#!/bin/sh\n[ \"$1\" = -n ] && shift\nexec \"$@\"\n"),
        (
            "mysql",
            "#!/bin/sh\nprintf 'd1\\t5000000000\\t3000000000\\t4000000000\\tSELECT * FROM `t` WHERE `id` = ?\\n'\nprintf 'd2\\t1000000000000\\t\\t\\tINSERT INTO `t` VALUES (...)\\n'\n",
        ),
    ] {
        let path = bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let prefix = directory.path().join("isuscope-t");
    // DIGEST, COUNT_STAR, SUM_TIMER_WAIT, SUM_LOCK_TIME, SUM_ROWS_SENT, SUM_ROWS_EXAMINED (picoseconds)
    fs::write(
        format!("{}.digest", prefix.display()),
        "T\t1001.0\nd1\t2\t2000000000\t0\t2\t10\n\
         T\t1004.5\nd1\t4\t4000000000\t0\t4\t20\nd2\t1\t1000000000000\t0\t0\t100\n\
         T\t1006.0\nd1\t10\t10000000000\t1000000\t10\t50\nd2\t1\t1000000000000\t0\t0\t100\n\
         T\t1069.0\nd1\t1000\t1000000000000\t2000000000\t1000\t5000\nd2\t1\t1000000000000\t0\t0\t100\n\
         T\t1071.0\nd1\t1100\t1100000000000\t2000000000\t1100\t5500\nd2\t1\t1000000000000\t0\t0\t100\n",
    )
    .unwrap();
    let script = template
        .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
        .replace("{benchmark_started_at}", "1000.0")
        .replace("{load_started_at}", "1005.0")
        .replace("{benchmark_finished_at}", "1070.0");
    let output = Command::new("sh")
        .args(["-c", &script])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let rows = stdout
        .lines()
        .filter(|line| !line.starts_with('{'))
        .map(|line| line.split('\t').collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let row = |window: &str, query_start: &str| {
        rows.iter()
            .find(|row| row[0] == window && row[2].starts_with(query_start))
            .unwrap_or_else(|| panic!("no {window} row for {query_start}:\n{stdout}"))
            .clone()
    };
    // initialize: the snapshot at 1004.5, the last one before the load start.
    let select = row("initialize", "SELECT");
    assert_eq!((select[1], select[3]), ("4", "0.004000000"));
    let insert = row("initialize", "INSERT");
    assert_eq!((insert[1], insert[3], insert[5]), ("1", "1.000000000", ""));
    // load: the snapshot at 1069.0 minus initialize; the one after the end (1071.0) is not used.
    let load = row("load", "SELECT");
    assert_eq!(
        (load[1], load[3], load[5], load[7], load[8], load[9]),
        (
            "996",
            "0.996000000",
            "0.003000000",
            "0.002000000",
            "996",
            "4980"
        )
    );
    assert!(
        !rows
            .iter()
            .any(|row| row[0] == "load" && row[2].starts_with("INSERT"))
    );
    let bucket_calls = stdout
        .lines()
        .filter(|line| line.contains("\"name\":\"db.calls\""))
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["value"]
                .as_f64()
                .unwrap()
        })
        .sum::<f64>();
    assert_eq!(bucket_calls, 1001.0);
    assert!(
        stdout.contains("\"name\":\"db.digest_snapshots\",\"value\":4"),
        "{stdout}"
    );
    // The node's temporary files are removed.
    assert!(!std::path::Path::new(&format!("{}.digest", prefix.display())).exists());
}

/// Server-side prepared statements (sqlx's binary protocol) never reach the digest table; the
/// collector reads `prepared_statements_instances` per SQL text instead. A statement whose
/// connection closed mid-run drops out of that table, and its counts must not go negative.
#[cfg(unix)]
#[test]
fn digest_collector_counts_prepared_statements_across_closed_connections() {
    use std::os::unix::fs::PermissionsExt;

    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions::default());
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    let collector = |name: &str| {
        config["collectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|collector| collector["name"].as_str() == Some(name))
            .unwrap()["command"][2]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(collector("mysql-digest-sampler").contains("prepared_statements_instances"));
    assert!(
        collector("mysql-digest-start")
            .contains("TRUNCATE TABLE performance_schema.prepared_statements_instances")
    );
    let template = collector("mysql-digest");

    let directory = tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for (name, body) in [
        ("sudo", "#!/bin/sh\n[ \"$1\" = -n ] && shift\nexec \"$@\"\n"),
        (
            "mysql",
            "#!/bin/sh\ncase \"$*\" in\n*prepared_statements_instances*) printf 'ps:a1\\t9000000000\\t\\t\\tSELECT * FROM user_present_all_received_history WHERE user_id=? AND present_all_id=?\\n' ;;\n*) : ;;\nesac\n",
        ),
    ] {
        let path = bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let prefix = directory.path().join("isuscope-t");
    // Two connections' rows summed per SQL text; at 1030 one connection closed and the sum fell.
    fs::write(
        format!("{}.digest", prefix.display()),
        "T\t1004.0\nps:a1\t1\t1000000000\t0\t1\t240000\tSELECT * FROM user_present_all_received_history WHERE user_id=? AND present_all_id=?\n\
         T\t1020.0\nps:a1\t100\t100000000000\t0\t100\t24000000\tSELECT * FROM user_present_all_received_history WHERE user_id=? AND present_all_id=?\n\
         T\t1025.0\nps:b2\t7\t7000000000\t0\t7\t7\tINSERT INTO user_sessions(id, user_id) VALUES (?, ?)\n\
         T\t1030.0\nps:a1\t40\t40000000000\t0\t40\t9600000\n\
         T\t1060.0\nps:a1\t90\t90000000000\t0\t90\t21600000\n",
    )
    .unwrap();
    let script = template
        .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
        .replace("{benchmark_started_at}", "1000.0")
        .replace("{load_started_at}", "1005.0")
        .replace("{benchmark_finished_at}", "1070.0");
    let output = Command::new("sh")
        .args(["-c", &script])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let load = stdout
        .lines()
        .map(|line| line.split('\t').collect::<Vec<_>>())
        .find(|row| row[0] == "load")
        .unwrap_or_else(|| panic!("no load row:\n{stdout}"));
    // 100 by 1020, then 90 more on the rows still open: 190 in all, 1 of them in initialize.
    assert_eq!(load[1], "189", "{stdout}");
    assert!(
        load[2].starts_with("SELECT * FROM user_present_all_received_history"),
        "{stdout}"
    );
    assert_eq!(
        load[4], "0.009000000",
        "the maximum comes from MAX_TIMER_EXECUTE"
    );
    assert_eq!(load[9], "45360000", "{stdout}");
    // b2's connection closed before the benchmark ended: its text comes from the snapshot.
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("load\t7\tINSERT INTO user_sessions(id, user_id)")),
        "{stdout}"
    );
}

/// The template `limits` collector reads the limits before the benchmark: nginx's own (not the
/// upstream's keepalive_requests), each service unit's open-file limit and MySQL's connections.
#[cfg(unix)]
#[test]
fn limits_collector_reads_the_limits_without_changing_anything() {
    use std::os::unix::fs::PermissionsExt;

    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions::default());
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    let template = config["collectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|collector| collector["name"].as_str() == Some("limits"))
        .unwrap()["command"][2]
        .as_str()
        .unwrap()
        .to_owned();
    let directory = tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for (name, body) in [
        ("sudo", "#!/bin/sh\n[ \"$1\" = -n ] && shift\nexec \"$@\"\n"),
        (
            "nginx",
            "#!/bin/sh\nprintf 'events {\\n    worker_connections 4096;\\n}\\nhttp {\\nupstream app {\\n  keepalive_requests 100000;\\n}\\n    keepalive_requests 10000;\\n}\\n'\n",
        ),
        ("pgrep", "#!/bin/sh\necho 2\n"),
        (
            "systemctl",
            "#!/bin/sh\n[ \"$3\" = LimitNOFILESoft ] && echo 1024 || echo 524288\n",
        ),
        ("mysql", "#!/bin/sh\nprintf 'max_connections\\t151\\n'\n"),
    ] {
        let path = bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = Command::new("sh")
        .args(["-c", &template.replace("{service_units}", "app.service")])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metrics = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .map(|value| {
            (
                value["name"].as_str().unwrap().to_owned(),
                value["value"].as_f64().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    for (name, value) in [
        ("limit.nginx_worker_connections", 4096.0),
        ("limit.nginx_keepalive_requests", 10000.0),
        ("limit.nginx_workers", 2.0),
        ("limit.service_nofile", 1024.0),
        ("limit.mysql_max_connections", 151.0),
    ] {
        assert!(
            metrics.iter().any(|(n, v)| n == name && *v == value),
            "{name} {value} missing: {metrics:?}"
        );
    }
}

/// Without a slow log, the slow-log collectors' preflights pass when statement digests carry the
/// SQL (practice-14-forth: six warnings on every doctor), and slp fails when neither source
/// exists, so a baseline without any SQL is not started.
#[cfg(unix)]
#[test]
fn slow_log_preflights_follow_whether_statement_digests_carry_the_sql() {
    use std::os::unix::fs::PermissionsExt;

    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions::default());
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    let preflight = |name: &str| {
        config["collectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|collector| collector["name"].as_str() == Some(name))
            .unwrap()["preflight"][2]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let directory = tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let run = |script: &str, digests: &str, slow_log: Option<&str>| {
        for (name, body) in [
            (
                "sudo",
                "#!/bin/sh\n[ \"$1\" = -n ] && shift\nexec \"$@\"\n".to_owned(),
            ),
            ("mysql", format!("#!/bin/sh\necho {digests}\n")),
            ("slp", "#!/bin/sh\nexit 0\n".to_owned()),
        ] {
            let path = bin.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let log = directory.path().join("slow.log");
        let _ = fs::remove_file(&log);
        if let Some(contents) = slow_log {
            fs::write(&log, contents).unwrap();
        }
        let script = script.replace("/var/log/mysql/mysql-slow.log", log.to_str().unwrap());
        let output = Command::new("sh")
            .args(["-c", &script])
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )
    };
    for name in ["slp", "mysql-log-mark"] {
        let script = preflight(name);
        let (code, message) = run(&script, "YES", None);
        assert_eq!(code, 0, "{name}: {message}");
        assert!(message.contains("statement digests"), "{name}: {message}");
    }
    let (code, message) = run(&preflight("slp"), "NO", None);
    assert_eq!(code, 1, "{message}");
    assert!(message.contains("no SQL would be recorded"), "{message}");
    assert_eq!(run(&preflight("mysql-log-mark"), "NO", None).0, 75);
    assert_eq!(
        run(&preflight("slp"), "NO", Some("")).0,
        0,
        "an existing slow log is used"
    );
}

/// The digest sampler writes to a file, not stdout, so a lost ssh session never stops it
/// (practice-14-forth: twenty leftover samplers per node used about 40% of the CPU). It stops
/// once the after collector or the cleanup has removed the snapshots it wrote.
#[cfg(unix)]
#[test]
fn digest_sampler_stops_when_its_snapshots_are_removed() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions::default());
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    let script = config["collectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|collector| collector["name"].as_str() == Some("mysql-digest-sampler"))
        .unwrap()["command"][2]
        .as_str()
        .unwrap()
        .to_owned();
    let directory = tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for (name, body) in [
        ("sudo", "#!/bin/sh\n[ \"$1\" = -n ] && shift\nexec \"$@\"\n"),
        ("mysql", "#!/bin/sh\nprintf 'd1\\t1\\t1\\t0\\t0\\t0\\n'\n"),
    ] {
        let path = bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let prefix = directory.path().join("isuscope-t");
    let snapshots = std::path::PathBuf::from(format!("{}.digest", prefix.display()));
    let mut sampler = Command::new("sh")
        .args([
            "-c",
            &script.replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap()),
        ])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !snapshots.exists() {
        assert!(Instant::now() < deadline, "the sampler wrote no snapshot");
        std::thread::sleep(Duration::from_millis(50));
    }
    fs::remove_file(&snapshots).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = sampler.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = sampler.kill();
            panic!("the sampler kept running after its snapshots were removed");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success());
    assert!(!snapshots.exists(), "the sampler recreated its snapshots");
}
