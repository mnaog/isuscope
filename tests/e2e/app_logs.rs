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
         F\t/var/lib/mysql/#innodb_redo/#ib_redo1\t0\t5000000000\t0\t4096\n\
         T\tisucon.user_presents\t2000000000\t0\t10\t0\n",
    )
    .unwrap();
    let after = |uptime: u32| {
        format!(
            "Uptime\t{uptime}\n\
             F\t/var/lib/mysql/isucon/user_presents.ibd\t52100000000000\t0\t475004928\t0\n\
             F\t/var/lib/mysql/#innodb_redo/#ib_redo1\t0\t9000000000\t0\t8192\n\
             F\t/var/lib/mysql/#innodb_redo/#ib_redo2\t0\t2000000000\t0\t4096\n\
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
