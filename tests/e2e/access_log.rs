use super::*;

/// collectorのstdoutを、順に依らない形（metricの行と、windowごとの表の行）に直す。
/// 同時接続数は接続ごとの足し算の順で最後の桁が変わり得るので、小数6桁の範囲で比べる。
/// 平均はalpが小数3桁に丸めていた（parserは合計と回数から求め直す）ので比べない。
fn summary_lines(raw: &str) -> (Vec<String>, Vec<(String, f64)>, Vec<String>) {
    let mut metrics = Vec::new();
    let mut connections = Vec::new();
    let mut tables = Vec::new();
    for line in raw.lines().filter(|line| !line.is_empty()) {
        if line.starts_with('{') {
            let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
            if value["name"] == "client.connections_in_use" {
                let number = value["value"].as_f64().unwrap();
                value.as_object_mut().unwrap().remove("value");
                connections.push((value.to_string(), number));
            } else {
                metrics.push(value.to_string());
            }
            continue;
        }
        let (window, json) = line.split_once('\t').unwrap();
        if window == "lines" {
            tables.push(format!("lines {json}"));
            continue;
        }
        let rows: Vec<Vec<serde_json::Value>> = serde_json::from_str(json).unwrap();
        let header = rows[0].clone();
        for row in &rows[1..] {
            let record = header
                .iter()
                .map(|key| key.as_str().unwrap().to_owned())
                .zip(row.iter().map(|value| match value.as_f64() {
                    // alpは整数になる値を`19`、isuscope-nodeは`19.0`と書くことがある。値で比べる。
                    Some(number) => serde_json::json!(number),
                    None => value.clone(),
                }))
                .filter(|(key, _)| key != "avg")
                .collect::<serde_json::Map<_, _>>();
            tables.push(format!("{window} {}", serde_json::Value::Object(record)));
        }
    }
    metrics.sort();
    connections.sort_by(|left, right| left.0.cmp(&right.0));
    tables.sort();
    (metrics, connections, tables)
}

fn assert_same_summary(expected: &str, actual: &str) {
    let (expected_metrics, expected_connections, expected_tables) = summary_lines(expected);
    let (metrics, connections, tables) = summary_lines(actual);
    assert_eq!(metrics, expected_metrics);
    assert_eq!(tables, expected_tables);
    assert_eq!(connections.len(), expected_connections.len());
    for ((key, value), (expected_key, expected_value)) in
        connections.iter().zip(&expected_connections)
    {
        assert_eq!(key, expected_key);
        assert!(
            (value - expected_value).abs() < 1e-5,
            "{key}: {value} != {expected_value}"
        );
    }
}

/// templateの`alp` collectorを、PATHに置いた`isuscope-node`（このisuscopeの`__access-summary`）で動かす。
#[cfg(unix)]
struct Collector {
    directory: tempfile::TempDir,
    preflight: String,
    command: String,
}

#[cfg(unix)]
impl Collector {
    fn new() -> Self {
        let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions::default());
        let config: toml::Value = toml::from_str(&rendered).unwrap();
        let collector = config["collectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|collector| collector["name"].as_str() == Some("alp"))
            .unwrap();
        let collector = Self {
            directory: tempdir().unwrap(),
            preflight: collector["preflight"][2].as_str().unwrap().to_owned(),
            command: collector["command"][2].as_str().unwrap().to_owned(),
        };
        fs::create_dir(collector.directory.path().join("bin")).unwrap();
        collector.node(&format!(
            "#!/bin/sh\nexec '{}' __access-summary \"$@\"\n",
            env!("CARGO_BIN_EXE_isuscope")
        ));
        collector
    }

    fn node(&self, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.directory.path().join("bin/isuscope-node");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn input(&self) -> std::path::PathBuf {
        self.directory.path().join("isuscope-t.nginx.log")
    }

    fn run(&self, template: &str, log: Option<&[u8]>) -> std::process::Output {
        if let Some(log) = log {
            fs::write(self.input(), log).unwrap();
        }
        let prefix = self.directory.path().join("isuscope-t");
        let script = template
            .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
            .replace("{benchmark_started_at}", "1789653660.000000")
            .replace("{load_started_at}", "1789653662.500000")
            .replace("{benchmark_finished_at}", "1789653670.000000")
            .replace("{route_matching_groups}", "^/user/[0-9]+/home$")
            .replace("{isuscope_node_build}", isuscope::access_summary::BUILD_ID);
        Command::new("sh")
            .args(["-c", &script])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.directory.path().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .output()
            .unwrap()
    }
}

/// 差分全体と5秒bucketの表、接続とupstreamの値。`alp-windows-v1.0.21.out`は同じlogを以前のawkと
/// alp 1.0.21で集計した出力で、このlogでは丸めと最小値の違いが出ないので、平均以外は同じになる。
#[cfg(unix)]
#[test]
fn alp_collector_aggregates_the_delta_with_isuscope_node() {
    let collector = Collector::new();
    let preflight = collector.run(&collector.preflight, None);
    assert!(preflight.status.success());
    assert!(
        String::from_utf8_lossy(&preflight.stdout).contains(isuscope::access_summary::BUILD_ID)
    );

    let output = collector.run(
        &collector.command,
        Some(include_bytes!("../fixtures/access-ltsv-windows.log")),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_same_summary(
        include_str!("../fixtures/alp-windows-v1.0.21.out"),
        &String::from_utf8_lossy(&output.stdout),
    );
    // 差分はnodeに残さない。
    assert!(!collector.input().exists());

    // 区間は[始まり, 終わり)。終わりちょうどの要求は時系列に入れない（差分全体には入る）。
    let output = collector.run(
        &collector.command,
        Some(b"time:2026-09-17T14:01:10+00:00\tmethod:GET\turi:/end\tstatus:200\treqtime:0.001\tconn:9\tmsec:1789653670.000\n"),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(stdout.contains("\"/end\""), "{stdout}");
    assert!(
        !stdout.lines().any(|line| line.starts_with("17896536")),
        "{stdout}"
    );

    // 行はあるのにmethodとuriを1件も読めない（LTSVでない）logは、設定不一致として失敗させる。
    let output = collector.run(
        &collector.command,
        Some(b"127.0.0.1 - - [17/Sep/2026:14:01:00 +0000] \"GET / HTTP/1.1\" 200 12\n"),
    );
    assert_eq!(output.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&output.stderr).contains("none of 1 access-log lines"));

    // 空の差分は失敗ではない。差分が無いnodeは集計しない。
    assert!(
        collector
            .run(&collector.command, Some(b""))
            .status
            .success()
    );
    assert_eq!(
        collector.run(&collector.command, None).status.code(),
        Some(75)
    );
}

/// 違うbuildか、無い`isuscope-node`では集計せずに失敗する（黙ってHTTPの計測が欠けないように）。
#[cfg(unix)]
#[test]
fn alp_collector_refuses_a_missing_or_different_isuscope_node() {
    let collector = Collector::new();
    collector.node("#!/bin/sh\necho 0000000000000000\n");
    let preflight = collector.run(&collector.preflight, None);
    assert_eq!(preflight.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&preflight.stdout).contains("rebuild it"));
    let output = collector.run(&collector.command, Some(b"method:GET\turi:/\n"));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("rebuild it"));
    // 失敗しても差分は消す。
    assert!(!collector.input().exists());

    fs::remove_file(collector.directory.path().join("bin/isuscope-node")).unwrap();
    let preflight = collector.run(&collector.preflight, None);
    assert_eq!(preflight.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&preflight.stdout).contains("not installed"));
    let output = collector.run(&collector.command, Some(b"method:GET\turi:/\n"));
    assert_eq!(output.status.code(), Some(1));
}

/// 崩れた行（`:`の無い項目、空の項目、壊れたescape、`-`の時間、範囲外のstatusなど）も止まらずに数える。
#[test]
fn node_summary_reads_broken_lines_without_stopping() {
    let options = isuscope::access_summary::Options {
        begin: Some(1_789_653_660.0),
        load_start: Some(1_789_653_662.5),
        end: Some(1_789_653_670.0),
        matching_groups: "^/user/[0-9]+/home$".into(),
    };
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/access-ltsv-edge.log");
    let mut output = Vec::new();
    let code =
        isuscope::access_summary::summarize(path.to_str().unwrap(), &options, &mut output).unwrap();
    assert_eq!(code, 0);
    let output = String::from_utf8(output).unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    // `lines`は、methodとuriの項目がある行の数。
    let expected = raw
        .lines()
        .filter(|line| {
            let keys = line
                .split('\t')
                .filter_map(|field| field.split_once(':').map(|(key, _)| key))
                .collect::<Vec<_>>();
            keys.contains(&"method") && keys.contains(&"uri")
        })
        .count();
    assert!(
        output.contains(&format!("\nlines\t{expected}\n")),
        "{output}"
    );
    for line in output.lines() {
        if line.starts_with('{') {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        } else {
            let (_, json) = line.split_once('\t').unwrap();
            serde_json::from_str::<serde_json::Value>(json).unwrap();
        }
    }
}
