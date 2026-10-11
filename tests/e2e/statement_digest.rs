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
