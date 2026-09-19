use super::*;

/// The template `slp` collector with a stand-in `slp`, on the awk this machine has. The real
/// slp 0.2.1 output under mawk and gawk is fixed in `tests/fixtures/slp-windows-v0.2.1.out`.
/// Statements are put into windows by when they started: `SET timestamp=` only has the second,
/// so the fraction comes from `# Time:` (when the statement was written) minus `Query_time`.
#[cfg(unix)]
#[test]
fn slp_collector_puts_statements_on_the_right_side_of_the_load_start() {
    use std::os::unix::fs::PermissionsExt;

    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions::default());
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    let template = config["collectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|collector| collector["name"].as_str() == Some("slp"))
        .unwrap()["command"][2]
        .as_str()
        .unwrap()
        .to_owned();
    // MySQL 8.0.46のslow logの1文（10:42:55.822396に書かれ、35 μsかかった）。
    let record = "# Time: 2026-08-27T10:42:55.822396Z\n# User@Host: root[root] @ localhost []  Id:     8\n# Query_time: 0.000035  Lock_time: 0.000000 Rows_sent: 1  Rows_examined: 1\nSET timestamp=1787827375;\nSELECT VERSION();\n";
    let run = |log: &str, load_start: &str| {
        let directory = tempdir().unwrap();
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let slp = bin.join("slp");
        fs::write(
            &slp,
            "#!/bin/sh\nprintf '1\\tSELECT VERSION()\\t0.000035\\t0.000035\\t0.000035\\t0.000035\\t0\\t1\\t1\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&slp, fs::Permissions::from_mode(0o755)).unwrap();
        let prefix = directory.path().join("isuscope-t");
        fs::write(format!("{}.mysql.log", prefix.display()), log).unwrap();
        let script = template
            .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
            .replace("{benchmark_started_at}", "")
            .replace("{load_started_at}", load_start)
            .replace("{benchmark_finished_at}", "");
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
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let window = stdout
            .lines()
            .find_map(|line| line.split_once('\t').map(|(window, _)| window.to_owned()))
            .filter(|window| !window.starts_with('{'))
            .unwrap_or_default();
        (window, stdout)
    };

    // 始まったのは10:42:55.822361。負荷の始まりがその前なら負荷、後ならinitialize。
    // `SET timestamp=`の秒（10:42:55）だけで比べると、どちらもinitializeになっていた。
    let (window, stdout) = run(record, "1787827375.800000");
    assert_eq!(window, "load", "{stdout}");
    assert!(!stdout.contains("db.slow_log_coarse_time"), "{stdout}");
    let (window, _) = run(record, "1787827375.823000");
    assert_eq!(window, "initialize");

    // `# Time:`が無ければ秒までしか分からない。秒で振り分け、その数を残す。
    let without_time = record
        .lines()
        .filter(|line| !line.starts_with("# Time:"))
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    let (window, stdout) = run(&without_time, "1787827375.800000");
    assert_eq!(window, "initialize");
    assert!(
        stdout.contains(r#""name":"db.slow_log_coarse_time","value":1"#),
        "{stdout}"
    );
}
