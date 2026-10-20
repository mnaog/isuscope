use super::*;

/// The template `alp` collector with a stand-in `alp`, on the awk this machine has. The real
/// alp 1.0.21 output under mawk and gawk is fixed in `tests/fixtures/alp-windows-v1.0.21.out`.
#[cfg(unix)]
#[test]
fn alp_collector_aggregates_the_delta_on_the_node() {
    use std::os::unix::fs::PermissionsExt;

    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions::default());
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    let template = config["collectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|collector| collector["name"].as_str() == Some("alp"))
        .unwrap()["command"][2]
        .as_str()
        .unwrap()
        .to_owned();
    let run = |log: &[u8]| {
        let directory = tempdir().unwrap();
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        // 渡されたfileの行数を数えるだけのalp。
        let alp = bin.join("alp");
        fs::write(
            &alp,
            "#!/bin/sh\nwhile [ \"$1\" != --file ]; do shift; done\nprintf '[[\"count\"],[%s]]\\n' \"$(wc -l < \"$2\" | tr -d ' ')\"\n",
        )
        .unwrap();
        fs::set_permissions(&alp, fs::Permissions::from_mode(0o755)).unwrap();
        // 違うbuildの`isuscope-node`は使わず、awkとalpで集計する。
        let node = bin.join("isuscope-node");
        fs::write(&node, "#!/bin/sh\necho 0000000000000000\n").unwrap();
        fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
        let prefix = directory.path().join("isuscope-t");
        fs::write(format!("{}.nginx.log", prefix.display()), log).unwrap();
        let script = template
            .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
            .replace("{benchmark_started_at}", "1789653660.000000")
            .replace("{load_started_at}", "1789653662.500000")
            .replace("{benchmark_finished_at}", "1789653670.000000")
            .replace("{route_matching_groups}", "")
            .replace("{isuscope_node_build}", isuscope::access_summary::BUILD_ID);
        let output = Command::new("sh")
            .args(["-c", &script])
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap();
        let left = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "bin")
            .count();
        (output, left)
    };

    let (output, left) = run(include_bytes!("../fixtures/access-ltsv-windows.log"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("aggregating with awk and alp"));
    // 差分と作業fileはnodeに残さない。
    assert_eq!(left, 0);
    // 差分全体（8行）と、ベンチ区間の5秒bucket（4行と2行）。bucketは負荷の始まり
    // （1789653662.5）から区切り、始まりをまたぐbucketを作らない。ベンチの始まり（1789653660）を
    // またぐbucketは、始まりを先頭にする。alpと比べるため、methodとuriを読めた行の数も出す。
    assert!(stdout.contains("\nlines\t8\n"), "{stdout}");
    assert!(stdout.contains("whole\t[[\"count\"],[8]]"), "{stdout}");
    assert!(
        stdout.contains("1789653660.000000\t[[\"count\"],[4]]"),
        "{stdout}"
    );
    assert!(
        stdout.contains("1789653662.500000\t[[\"count\"],[2]]"),
        "{stdout}"
    );
    for expected in [
        r#""name":"client.connections_opened_total","value":3.000000"#,
        r#""name":"client.connection_requests_max","value":3.000000"#,
        r#""name":"http.upstream_retried_requests","value":1.000000,"unit":"requests","labels":{"upstream":"10.0.0.9:8080"}"#,
        r#""name":"http.upstream_response_duration_max","value":2.000000,"unit":"ms","labels":{"upstream":"10.0.0.9:8080"}"#,
        r#""name":"http.upstream_connect_duration_max","value":1.000000,"unit":"ms","labels":{"upstream":"10.0.0.1:8080"}"#,
        r#""name":"client.request_gap","value":5360.000134,"unit":"ms","labels":{"quantile":"0.99"},"timestamp":1789653662.500000"#,
    ] {
        assert!(stdout.contains(expected), "missing {expected} in {stdout}");
    }

    // 行はあるのにmethodとuriを1件も読めない（LTSVでない）logは、設定不一致として失敗させる。
    let (output, _) =
        run(b"127.0.0.1 - - [17/Sep/2026:14:01:00 +0000] \"GET / HTTP/1.1\" 200 12\n");
    assert_eq!(output.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&output.stderr).contains("none of 1 access-log lines"));

    // 区間は[始まり, 終わり)。終わりちょうどの要求は時系列に入れない（差分全体には入る）。
    let (output, _) = run(b"time:2026-09-17T14:01:10+00:00\tmethod:GET\turi:/end\tstatus:200\treqtime:0.001\tconn:9\tmsec:1789653670.000\n");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(stdout.contains("whole\t[[\"count\"],[1]]"), "{stdout}");
    assert!(
        !stdout.lines().any(|line| line.starts_with("17896536")),
        "{stdout}"
    );

    // 空の差分は失敗ではない。
    let (output, _) = run(b"");
    assert!(output.status.success());
}

/// collectorのstdoutを、順に依らない形（metricの行と、windowごとのalpの表の行）に直す。
/// 同時接続数は接続ごとの足し算の順がawkのhashの順に依るので、小数6桁の範囲で比べる。
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

/// `isuscope-node`の集計は、awkとalp 1.0.21で集計した出力と同じ。
#[test]
fn node_summary_matches_the_awk_and_alp_aggregation() {
    let options = isuscope::access_summary::Options {
        begin: Some(1_789_653_660.0),
        load_start: Some(1_789_653_662.5),
        end: Some(1_789_653_670.0),
        matching_groups: "^/user/[0-9]+/home$".into(),
    };
    for (log, expected) in [
        (
            "tests/fixtures/access-ltsv-windows.log",
            include_str!("../fixtures/alp-windows-v1.0.21.out"),
        ),
        (
            "tests/fixtures/access-ltsv-edge.log",
            include_str!("../fixtures/alp-edge-v1.0.21.out"),
        ),
    ] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(log);
        let mut output = Vec::new();
        let code =
            isuscope::access_summary::summarize(path.to_str().unwrap(), &options, &mut output)
                .unwrap();
        assert_eq!(code, 0, "{log}");
        assert_same_summary(expected, &String::from_utf8(output).unwrap());
    }

    // 行はあるのにmethodとuriを1件も読めない（LTSVでない）logは、設定不一致として65で終える。
    let directory = tempdir().unwrap();
    let path = directory.path().join("combined.log");
    fs::write(
        &path,
        "127.0.0.1 - - [17/Sep/2026:14:01:00 +0000] \"GET / HTTP/1.1\" 200 12\n",
    )
    .unwrap();
    let code =
        isuscope::access_summary::summarize(path.to_str().unwrap(), &options, &mut Vec::new())
            .unwrap();
    assert_eq!(code, 65);
}

/// templateの`alp` collectorは、同じbuildの`isuscope-node`があればそれで集計し、差分を消す。
#[cfg(unix)]
#[test]
fn alp_collector_uses_isuscope_node_of_the_same_build() {
    use std::os::unix::fs::PermissionsExt;

    let rendered = isuscope::init::render_config(&isuscope::init::ConfigOptions::default());
    let config: toml::Value = toml::from_str(&rendered).unwrap();
    let collector = config["collectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|collector| collector["name"].as_str() == Some("alp"))
        .unwrap();
    let directory = tempdir().unwrap();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let node = bin.join("isuscope-node");
    fs::write(
        &node,
        format!(
            "#!/bin/sh\nexec '{}' __access-summary \"$@\"\n",
            env!("CARGO_BIN_EXE_isuscope")
        ),
    )
    .unwrap();
    fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
    let prefix = directory.path().join("isuscope-t");
    let input = format!("{}.nginx.log", prefix.display());
    fs::write(
        &input,
        include_bytes!("../fixtures/access-ltsv-windows.log"),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let render = |script: &str| {
        script
            .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
            .replace("{benchmark_started_at}", "1789653660.000000")
            .replace("{load_started_at}", "1789653662.500000")
            .replace("{benchmark_finished_at}", "1789653670.000000")
            .replace("{route_matching_groups}", "^/user/[0-9]+/home$")
            .replace("{isuscope_node_build}", isuscope::access_summary::BUILD_ID)
    };

    let preflight = Command::new("sh")
        .args(["-c", &render(collector["preflight"][2].as_str().unwrap())])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(preflight.status.success());
    assert!(
        String::from_utf8_lossy(&preflight.stdout).contains(isuscope::access_summary::BUILD_ID)
    );

    let output = Command::new("sh")
        .args(["-c", &render(collector["command"][2].as_str().unwrap())])
        .env("PATH", &path)
        .output()
        .unwrap();
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
    assert!(!std::path::Path::new(&input).exists());

    // buildが違えば、preflightは何を置き直すかを示して失敗する。
    fs::write(&node, "#!/bin/sh\necho 0000000000000000\n").unwrap();
    let stale = Command::new("sh")
        .args(["-c", &render(collector["preflight"][2].as_str().unwrap())])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert_eq!(stale.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&stale.stdout).contains("rebuild it"));
}
