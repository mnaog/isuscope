use super::*;

/// 型の違うnodeが混ざる構成（c7iとc6in）で、briefの`hosts`がnodeごとの型とCPUを並べ、
/// 混ざっていることを`warnings`で伝える。値の分からないnodeには付けない。
#[test]
fn hosts_carry_each_node_instance_type_and_cpu_from_the_fingerprint() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "sleep 2; printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":1,\"pass\":true}'"]

# remote-fingerprintとhost-samplerがnodeごとに出す行と同じ形。
[[collectors]]
name = "remote-fingerprint"
phase = "before"
transport = "local"
command = ["sh", "-c", """
fp() { printf '{"type":"fingerprint","name":"%s","value":"%s","labels":{"node":"%s"}}\n' "$1" "$2" "$3"; }
fp instance.type c7i.large app1
fp cpu.model 'Xeon 8488C' app1
fp instance.type c6in.large app2
fp cpu.model 'Xeon 8375C' app2
fp instance.type unknown app3
"""]

[[collectors]]
name = "host-sampler"
phase = "after"
transport = "local"
command = ["sh", "-c", """
for node in app1 app2 app3; do
  printf '{"type":"metric","name":"host.cpu_busy_percent","value":50,"unit":"percent","labels":{"node":"%s"},"timestamp":{benchmark_started_at}}\n' "$node"
done
"""]
"#,
    )
    .unwrap();
    let isuscope = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
            .args(args)
            .current_dir(project.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        parsed(&output.stdout).unwrap_or_default()
    };
    isuscope(&["run", "--hypothesis", "mixed types"]);
    let brief = isuscope(&["brief", "latest"]);
    let hosts = brief["hosts"]["rows"].as_array().unwrap();
    let host = |name: &str| hosts.iter().find(|row| row["node"] == name).unwrap();
    assert_eq!(host("app1")["instance_type"], "c7i.large", "{hosts:?}");
    assert_eq!(host("app1")["cpu_model"], "Xeon 8488C");
    assert_eq!(host("app2")["instance_type"], "c6in.large");
    assert!(host("app3")["instance_type"].is_null(), "{hosts:?}");
    let warnings = brief["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .unwrap()
            .contains("different instance types: c6in.large (app2) / c7i.large (app1)")),
        "{warnings:?}"
    );
}
