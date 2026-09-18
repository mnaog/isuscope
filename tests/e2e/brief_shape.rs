use super::*;

fn project_with_host_metrics() -> tempfile::TempDir {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    // 2 nodeぶんのhost metricと、coreやquantileで分かれるmetricを出すcollector。
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":100,\"pass\":true}'"]

[[collectors]]
name = "host-sampler"
phase = "during"
transport = "local"
command = ["sh", "-c", """
for node in app1 app2; do
  printf '{"type":"metric","name":"host.cpu_busy_percent","value":20,"unit":"percent","labels":{"node":"%s"}}\n' "$node"
  printf '{"type":"metric","name":"host.core_busy_percent","value":100,"unit":"percent","labels":{"node":"%s","core":"0"}}\n' "$node"
  printf '{"type":"metric","name":"host.core_busy_percent","value":0,"unit":"percent","labels":{"node":"%s","core":"1"}}\n' "$node"
  printf '{"type":"metric","name":"service.cpu_cores","value":0.5,"unit":"cores","labels":{"node":"%s","service":"app.service"}}\n' "$node"
done
"""]
"#,
    )
    .unwrap();
    project
}

#[test]
fn brief_shows_every_node_and_keeps_core_labels_apart() {
    let project = project_with_host_metrics();
    let run = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "全nodeが1行で出る"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["brief", "latest"])
        .current_dir(project.path())
        .output()
        .unwrap();
    let brief: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    // hostsは1 node 1行で、node数が既定の表示件数を超えても全nodeが出る。
    let hosts = brief["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 2, "{}", brief["hosts"]);
    let nodes = hosts
        .iter()
        .map(|host| host["node"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(nodes, ["app1", "app2"]);
    for host in hosts {
        assert_eq!(host["cpu_busy_percent"], 20.0);
        // 平均50%ではなく、最も詰まっていたコアが出る。
        assert_eq!(host["busiest_core_peak_percent"], 100.0);
        assert_eq!(host["top_services"][0]["service"], "app.service");
        assert!(host["detail_rows"].as_u64().unwrap() >= 3);
    }
}

#[test]
fn a_comparison_states_what_changed_between_the_two_runs() {
    let project = project_with_host_metrics();
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
        output.stdout
    };
    isuscope(&["run", "--hypothesis", "base", "--tag", "bench:v1"]);
    let base: serde_json::Value =
        serde_json::from_slice(&isuscope(&["list", "--limit", "1"])).unwrap();
    let base_id = base["runs"][0]["id"].as_str().unwrap().to_owned();
    isuscope(&[
        "analyze",
        &base_id,
        "inconclusive",
        "--analysis",
        "比較元として記録する",
    ]);
    isuscope(&["run", "--hypothesis", "candidate", "--tag", "bench:v2"]);
    let candidate: serde_json::Value =
        serde_json::from_slice(&isuscope(&["list", "--limit", "1"])).unwrap();
    let candidate_id = candidate["runs"][0]["id"].as_str().unwrap().to_owned();
    isuscope(&[
        "analyze",
        &candidate_id,
        "inconclusive",
        "--analysis",
        "比較の前提を確認する",
        "--base",
        &base_id,
    ]);

    let brief: serde_json::Value =
        serde_json::from_slice(&isuscope(&["brief", &candidate_id])).unwrap();
    let conditions = brief["review"]["comparison"]["conditions"]
        .as_array()
        .unwrap();
    let state = |name: &str| {
        conditions
            .iter()
            .find(|condition| condition["name"] == name)
            .unwrap_or_else(|| panic!("{name} is missing from {conditions:?}"))["state"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(state("observation"), "same");
    // ベンチ条件はtagでしか分からない。印が違えばchanged、無ければunknown。
    assert_eq!(state("benchmark"), "changed");
    // fingerprintを取っていないrunは「同じ」ではなく「不明」。
    assert_eq!(state("environment"), "unknown");
}
