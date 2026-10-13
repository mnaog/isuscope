use super::*;

/// 配布するcollectorのscriptを取り出し、一時directoryで動くようにpathとplaceholderを置き換える。
#[cfg(unix)]
fn template_script(name: &str, access_log: &std::path::Path, prefix: &std::path::Path) -> String {
    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions {
        nginx_access_log: access_log.display().to_string(),
        service_units: vec!["isu.service".into()],
        ..Default::default()
    });
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    config["collectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|collector| collector["name"].as_str() == Some(name))
        .unwrap()["command"][2]
        .as_str()
        .unwrap()
        .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
        .replace("{service_units}", "isu.service")
}

#[cfg(unix)]
fn stub(bin: &std::path::Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = bin.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn run_script(script: &str, bin: &std::path::Path, env: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new("sh");
    command.args(["-c", script]).env(
        "PATH",
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
    );
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

#[cfg(unix)]
fn metrics(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[cfg(unix)]
fn value(metrics: &[serde_json::Value], name: &str, label: (&str, &str)) -> Option<f64> {
    metrics
        .iter()
        .find(|metric| metric["name"] == name && metric["labels"][label.0] == label.1)
        .and_then(|metric| metric["value"].as_f64())
}

/// Performance Schemaの累積値の前後の差を、file・tableごとのmetricにする。
#[cfg(unix)]
#[test]
fn mysql_io_collector_reports_the_waits_between_the_two_snapshots() {
    let directory = tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    stub(&bin, "sudo", "[ \"$1\" = -n ] && shift\nexec \"$@\"\n");
    stub(&bin, "mysql", "cat \"$MYSQL_FIXTURE\"\n");
    let prefix = directory.path().join("isuscope-t");
    let access_log = directory.path().join("access.log");
    let mark = template_script("mysql-io-mark", &access_log, &prefix);
    let delta = template_script("mysql-io-delta", &access_log, &prefix);

    // 時間はps。user_presents.ibdの読み込み待ちは1,000msから52,100msへ増える。
    let before = directory.path().join("before.tsv");
    fs::write(
        &before,
        "Uptime\t100\n\
         F\t/var/lib/mysql/isucon/user_presents.ibd\t1000000000000\t0\t1048576\t0\n\
         F\t/var/lib/mysql/#innodb_redo/#ib_redo1\t0\t5000000000\t0\t4096\t1000000000\n\
         T\tisucon.user_presents\t2000000000\t0\t10\t0\n",
    )
    .unwrap();
    let after = |uptime: u32| {
        format!(
            "Uptime\t{uptime}\n\
             F\t/var/lib/mysql/isucon/user_presents.ibd\t52100000000000\t0\t475004928\t0\n\
             F\t/var/lib/mysql/#innodb_redo/#ib_redo1\t0\t9000000000\t0\t8192\t8000000000\n\
             F\t/var/lib/mysql/#innodb_redo/#ib_redo2\t0\t2000000000\t0\t4096\t3000000000\n\
             F\t/var/lib/mysql/isucon/users.ibd\t3000000000\t0\t16384\t0\n\
             F\t/var/lib/mysql/#ib_16384_0.dblwr\t0\t0\t0\t0\n\
             T\tisucon.user_presents\t5000000000\t1000000000\t30\t4\n\
             datadir\t/var/lib/mysql/\n\
             innodb_buffer_pool_size\t134217728\n\
             S\tisucon.user_presents\t563085312\n\
             S\tisucon.users\t16384\n"
        )
    };
    let after_path = directory.path().join("after.tsv");
    fs::write(&after_path, after(160)).unwrap();

    let output = run_script(&mark, &bin, &[("MYSQL_FIXTURE", before.to_str().unwrap())]);
    assert!(output.status.success(), "{output:?}");
    let output = run_script(
        &delta,
        &bin,
        &[("MYSQL_FIXTURE", after_path.to_str().unwrap())],
    );
    assert!(output.status.success(), "{output:?}");
    let metrics = metrics(&output.stdout);
    let file = ("file", "isucon/user_presents.ibd");
    assert_eq!(value(&metrics, "db.file.read_wait", file), Some(51_100.0));
    assert_eq!(
        value(&metrics, "db.file.read_bytes", file),
        Some(452.0 * 1_048_576.0)
    );
    // 前に無かったfileは0から数え、変わらなかったfileは出さない。
    assert_eq!(
        value(&metrics, "db.file.read_wait", ("file", "isucon/users.ibd")),
        Some(3.0)
    );
    assert!(
        !metrics
            .iter()
            .any(|metric| metric["labels"]["file"] == "#innodb_dblwr"),
        "{metrics:?}"
    );
    // redo logは番号ごとのfileを1行にまとめる（4ms + 新しいfileの2ms）。
    assert_eq!(
        value(&metrics, "db.file.write_wait", ("file", "#innodb_redo")),
        Some(6.0)
    );
    // fsyncなど読み書き以外の待ちも同じようにまとめる（7ms + 新しいfileの3ms）。古い形の行は0から数える。
    assert_eq!(
        value(&metrics, "db.file.misc_wait", ("file", "#innodb_redo")),
        Some(10.0)
    );
    assert_eq!(
        value(&metrics, "db.file.misc_wait", ("file", "isucon/users.ibd")),
        Some(0.0)
    );
    assert!(
        !metrics.iter().any(|metric| metric["labels"]["file"]
            .as_str()
            .is_some_and(|file| file.contains("#ib_redo"))),
        "{metrics:?}"
    );
    let table = ("table", "isucon.user_presents");
    assert_eq!(value(&metrics, "db.table.read_time", table), Some(3.0));
    assert_eq!(value(&metrics, "db.table.write_time", table), Some(1.0));
    assert_eq!(value(&metrics, "db.table.rows_read", table), Some(20.0));
    assert_eq!(value(&metrics, "db.table.rows_written", table), Some(4.0));
    assert_eq!(
        value(&metrics, "db.memory.buffer_pool", ("engine", "mysql")),
        Some(134_217_728.0)
    );
    assert_eq!(
        value(&metrics, "db.memory.tables", ("engine", "mysql")),
        Some(563_101_696.0)
    );

    // 間でmysqldが再起動すると累積値が0から数え直されるので、差は出さない。
    fs::write(&after_path, after(5)).unwrap();
    let output = run_script(&mark, &bin, &[("MYSQL_FIXTURE", before.to_str().unwrap())]);
    assert!(output.status.success(), "{output:?}");
    let output = run_script(
        &delta,
        &bin,
        &[("MYSQL_FIXTURE", after_path.to_str().unwrap())],
    );
    assert_eq!(output.status.code(), Some(75), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("restarted"));

    // Performance Schemaが無効ならfileの行が無いので、markの時点でunavailable。
    fs::write(&before, "Uptime\t100\n").unwrap();
    let output = run_script(&mark, &bin, &[("MYSQL_FIXTURE", before.to_str().unwrap())]);
    assert_eq!(output.status.code(), Some(75), "{output:?}");
}

/// ベンチの前に記録した位置より後の行だけを、型ごとに数える。
#[cfg(unix)]
#[test]
fn app_log_collector_counts_error_shapes_written_after_the_mark() {
    let directory = tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    stub(&bin, "sudo", "[ \"$1\" = -n ] && shift\nexec \"$@\"\n");
    // BSDのstatには`-c`が無いので、大きさだけ本物を返す。
    stub(
        &bin,
        "stat",
        "for last; do :; done\ntest -e \"$last\" || exit 1\necho \"1 2 $(wc -c < \"$last\" | tr -d ' ')\"\n",
    );
    // cursorより後を求められたときだけ、unitごとの行を返す。
    stub(
        &bin,
        "journalctl",
        r#"case " $* " in
  *" --show-cursor "*) printf 'last line before the run\n-- cursor: s=abc;i=1\n'; exit 0 ;;
esac
case " $* " in *" --after-cursor=s=abc;i=1 "*) ;; *) exit 0 ;; esac
case " $* " in
  *" -u isu.service "*)
    echo "GET /api/user/1 200 12ms"
    echo "GET /api/user/2 200 15ms"
    for id in 7 14 21; do echo "ERROR sqlx: Lock wait timeout exceeded; try restarting transaction user_id=$id"; done
    echo "WARN 統計の更新に失敗しました: セッション 01a1017e-d070 が見つかりません"
    ;;
  *" -k "*) echo "Out of memory: Killed process 4321 (mysqld) total-vm:2048000kB" ;;
esac
"#,
    );
    let logs = directory.path().join("nginx");
    fs::create_dir(&logs).unwrap();
    let access_log = logs.join("access.log");
    let error_log = logs.join("error.log");
    fs::write(
        &error_log,
        "2026/10/07 10:00:00 [error] 1#1: *1 connect() failed before the run\n",
    )
    .unwrap();
    let prefix = directory.path().join("isuscope-t");
    let output = run_script(
        &template_script("app-log-mark", &access_log, &prefix),
        &bin,
        &[],
    );
    assert!(output.status.success(), "{output:?}");

    let mut error = fs::OpenOptions::new()
        .append(true)
        .open(&error_log)
        .unwrap();
    use std::io::Write;
    for id in [3, 4] {
        writeln!(error, "2026/10/07 10:22:0{id} [error] {id}#{id}: *{id}7 upstream timed out (110: Connection timed out) while reading response header from upstream").unwrap();
    }
    drop(error);
    let output = run_script(
        &template_script("app-log-delta", &access_log, &prefix),
        &bin,
        &[],
    );
    assert!(output.status.success(), "{output:?}");
    let metrics = metrics(&output.stdout);
    let shape = |source: &str, pattern: &str| {
        metrics
            .iter()
            .find(|metric| {
                metric["name"] == "log.error_lines"
                    && metric["labels"]["source"] == source
                    && metric["labels"]["pattern"].as_str() == Some(pattern)
            })
            .map(|metric| {
                (
                    metric["value"].as_f64().unwrap(),
                    metric["labels"]["example"].as_str().unwrap().to_owned(),
                )
            })
    };
    // IDの違う3行は1つの型。通常のaccess logの行は数えない。
    let (count, example) = shape(
        "isu.service",
        "ERROR sqlx: Lock wait timeout exceeded; try restarting transaction user_id=<N>",
    )
    .unwrap();
    assert_eq!(count, 3.0);
    assert!(example.ends_with("user_id=7"), "{example}");
    assert!(
        shape(
            "isu.service",
            "WARN 統計の更新に失敗しました: セッション <N> が見つかりません"
        )
        .is_some(),
        "{metrics:?}"
    );
    assert_eq!(
        value(&metrics, "log.lines", ("source", "isu.service")),
        Some(6.0)
    );
    assert!(
        shape(
            "kernel",
            "Out of memory: Killed process <N> (mysqld) total-vm:<N>"
        )
        .is_some()
    );
    // error logはmarkの後に足された2行だけ。日付は型から外す。
    let (count, example) = shape(
        "nginx-error",
        "[error] <N>#<N>: *<N> upstream timed out (<N>: Connection timed out) while reading response header from upstream",
    )
    .unwrap();
    assert_eq!(count, 2.0);
    assert!(example.starts_with("[error] 3#3:"), "{example}");
    assert_eq!(
        value(&metrics, "log.lines", ("source", "nginx-error")),
        Some(2.0)
    );
    assert!(!prefix.with_extension("app-log.marker").exists());
}

/// unitのprocessのRSSとthread数を/procから読み、MainPIDが変わったら再起動として数える。
/// cgroupが見つからない環境（v1でsystemd階層が無いなど）ではMainPIDだけを見る。
#[cfg(target_os = "linux")]
#[test]
fn process_sampler_reads_rss_threads_and_counts_main_pid_changes() {
    let directory = tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let main = directory.path().join("main-pid");
    stub(
        &bin,
        "systemctl",
        "case \"$*\" in *MainPID*) cat \"$MAIN_PID_FILE\" ;; esac\n",
    );
    let script = template_script(
        "process-sampler",
        &directory.path().join("access.log"),
        &directory.path().join("isuscope-t"),
    );
    let mut first = Command::new("sleep").arg("30").spawn().unwrap();
    let mut second = Command::new("sleep").arg("30").spawn().unwrap();
    fs::write(&main, first.id().to_string()).unwrap();
    let sampler = Command::new("sh")
        .args(["-c", &script])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("MAIN_PID_FILE", &main)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    fs::write(&main, second.id().to_string()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    // isuscopeはduring collectorをSIGTERMで止める。
    Command::new("kill")
        .args(["-TERM", &sampler.id().to_string()])
        .status()
        .unwrap();
    let output = sampler.wait_with_output().unwrap();
    for child in [&mut first, &mut second] {
        child.kill().ok();
        child.wait().ok();
    }
    let metrics = metrics(&output.stdout);
    let service = ("service", "isu.service");
    assert!(value(&metrics, "service.process_rss_bytes", service).is_some_and(|rss| rss > 0.0));
    assert_eq!(
        value(&metrics, "service.process_threads", service),
        Some(1.0)
    );
    assert_eq!(value(&metrics, "service.processes", service), Some(1.0));
    assert_eq!(
        metrics
            .iter()
            .filter(|metric| metric["name"] == "service.process_restarts")
            .count(),
        1,
        "{metrics:?}"
    );
}

/// ENAの`*_allowance_exceeded`の前後の差をdevice・counterごとに出す。0も出し、
/// `conntrack_allowance_available`は後の値をそのまま出す。counterを持たないNICだけならunavailable。
#[cfg(target_os = "linux")]
#[test]
fn network_allowance_collector_reports_exceeded_packets_between_the_snapshots() {
    let directory = tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let counters = directory.path().join("counters");
    // loは読まない。どのdeviceにも同じcounterを返す。
    stub(
        &bin,
        "ethtool",
        "[ \"$2\" = lo ] && exit 1\ncat \"$COUNTERS\"\n",
    );
    let prefix = directory.path().join("isuscope-t");
    let access_log = directory.path().join("access.log");
    let mark = template_script("network-allowance-mark", &access_log, &prefix);
    let delta = template_script("network-allowance-delta", &access_log, &prefix);
    let env = [("COUNTERS", counters.to_str().unwrap())];

    fs::write(
        &counters,
        "NIC statistics:\n     tx_timeout: 0\n     bw_in_allowance_exceeded: 10\n     bw_out_allowance_exceeded: 100\n     pps_allowance_exceeded: 0\n     conntrack_allowance_available: 136000\n",
    )
    .unwrap();
    let output = run_script(&mark, &bin, &env);
    assert!(output.status.success(), "{output:?}");
    fs::write(
        &counters,
        "NIC statistics:\n     tx_timeout: 3\n     bw_in_allowance_exceeded: 10\n     bw_out_allowance_exceeded: 1334\n     pps_allowance_exceeded: 0\n     conntrack_allowance_available: 135000\n",
    )
    .unwrap();
    let output = run_script(&delta, &bin, &env);
    assert!(output.status.success(), "{output:?}");
    let metrics = metrics(&output.stdout);
    let device = fs::read_dir("/sys/class/net")
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .find(|name| name != "lo")
        .unwrap();
    let exceeded = |counter: &str| {
        metrics
            .iter()
            .find(|metric| {
                metric["name"] == "net.allowance_exceeded"
                    && metric["labels"]["device"] == device.as_str()
                    && metric["labels"]["counter"] == counter
            })
            .and_then(|metric| metric["value"].as_f64())
    };
    assert_eq!(exceeded("bw_out_allowance_exceeded"), Some(1234.0));
    assert_eq!(exceeded("bw_in_allowance_exceeded"), Some(0.0));
    assert_eq!(exceeded("pps_allowance_exceeded"), Some(0.0));
    assert_eq!(
        value(
            &metrics,
            "net.conntrack_allowance_available",
            ("device", device.as_str())
        ),
        Some(135_000.0)
    );
    assert!(
        !metrics
            .iter()
            .any(|metric| metric["labels"]["counter"] == "tx_timeout"),
        "{metrics:?}"
    );

    // ENA以外のNICはcounterを持たないので、markの時点でunavailable。
    fs::write(&counters, "NIC statistics:\n     rx_packets: 5\n").unwrap();
    let output = run_script(&mark, &bin, &env);
    assert_eq!(output.status.code(), Some(75), "{output:?}");
}
