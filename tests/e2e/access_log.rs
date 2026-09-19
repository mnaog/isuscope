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
        let prefix = directory.path().join("isuscope-t");
        fs::write(format!("{}.nginx.log", prefix.display()), log).unwrap();
        let script = template
            .replace("/tmp/isuscope-{run_id}", prefix.to_str().unwrap())
            .replace("{benchmark_started_at}", "1789653660.000")
            .replace("{benchmark_finished_at}", "1789653670.000")
            .replace("{route_matching_groups}", "");
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
    // 差分と作業fileはnodeに残さない。
    assert_eq!(left, 0);
    // 差分全体（8行）と、ベンチ区間の5秒bucket（4行と2行）。
    assert!(stdout.contains("whole\t[[\"count\"],[8]]"), "{stdout}");
    assert!(stdout.contains("1789653660\t[[\"count\"],[4]]"), "{stdout}");
    assert!(stdout.contains("1789653665\t[[\"count\"],[2]]"), "{stdout}");
    for expected in [
        r#""name":"client.connections_opened_total","value":3.000000"#,
        r#""name":"client.connection_requests_max","value":3.000000"#,
        r#""name":"http.upstream_retried_requests","value":1.000000,"unit":"requests","labels":{"upstream":"10.0.0.1:8080"}"#,
        r#""name":"http.upstream_connect_duration_max","value":3.000000,"unit":"ms","labels":{"upstream":"10.0.0.1:8080"}"#,
        r#""name":"client.request_gap","value":5360.000134,"unit":"ms","labels":{"quantile":"0.99"},"timestamp":1789653665"#,
    ] {
        assert!(stdout.contains(expected), "missing {expected} in {stdout}");
    }

    // 行はあるのにmethodとuriを1件も読めない（LTSVでない）logは、設定不一致として失敗させる。
    let (output, _) =
        run(b"127.0.0.1 - - [17/Sep/2026:14:01:00 +0000] \"GET / HTTP/1.1\" 200 12\n");
    assert_eq!(output.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&output.stderr).contains("none of 1 access-log lines"));

    // 空の差分は失敗ではない。
    let (output, _) = run(b"");
    assert!(output.status.success());
}
