use super::*;

/// `[include] config`のfileは`config.toml`の後に重なり、runの設定hashにも入る。
#[test]
fn included_config_overrides_the_generated_one_and_changes_the_config_hash() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[include]
config = [".isuscope/policy.toml"]

[benchmark]
mode = "command"
command = ["sh", "-c", "printf generated > ran; printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":1,\"pass\":false}'"]
"#,
    )
    .unwrap();
    let policy = |score: u32| {
        fs::write(
            config_dir.join("policy.toml"),
            format!(
                r#"
[benchmark]
command = ["sh", "-c", "printf policy > ran; printf '%s\n' '{{\"type\":\"isuscope.result\",\"score\":{score},\"pass\":false}}'"]
"#
            ),
        )
        .unwrap()
    };
    policy(7);
    let run = || {
        let output = Command::new(env!("CARGO_BIN_EXE_isuscope"))
            .args(["run", "--hypothesis", "policy wins"])
            .current_dir(project.path())
            .output()
            .unwrap();
        assert!(
            output.status.success() || output.status.code() == Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let list: serde_json::Value = parsed(
            &Command::new(env!("CARGO_BIN_EXE_isuscope"))
                .args([
                    "sql",
                    "SELECT id FROM runs ORDER BY started_at DESC LIMIT 1",
                ])
                .current_dir(project.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let id = list["rows"][0]["id"].as_str().unwrap().to_owned();
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(config_dir.join("runs").join(&id).join("run.json")).unwrap(),
        )
        .unwrap();
        (id, manifest)
    };
    let (first, manifest) = run();
    assert_eq!(
        fs::read_to_string(project.path().join("ran")).unwrap(),
        "policy"
    );
    assert_eq!(manifest["benchmark"]["score"], 7);
    // 重ねたfileの写しもrunに残る。
    let tooling = config_dir.join("runs").join(&first).join("tooling");
    assert!(
        fs::read_to_string(tooling.join("include/0-policy.toml"))
            .unwrap()
            .contains("score\\\":7"),
    );
    let first_hash = manifest["tooling"]["config_sha256"].clone();
    policy(8);
    let (_, manifest) = run();
    assert_ne!(manifest["tooling"]["config_sha256"], first_hash);
}

/// `--project`と`ISUSCOPE_PROJECT`で、別のdirectoryからprojectのrunを読める。書き込むcommandは
/// 今のdirectoryと同じprojectを指すときだけ受け付ける。
#[test]
fn project_option_reads_another_directorys_project() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "printf '%s\n' '{\"type\":\"isuscope.result\",\"score\":42,\"pass\":true}'"]
"#,
    )
    .unwrap();
    let ran = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["run", "--hypothesis", "read me from elsewhere"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(ran.status.success());
    let elsewhere = tempdir().unwrap();
    let isuscope = |args: &[&str], env: Option<&std::path::Path>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_isuscope"));
        command
            .args(args)
            .current_dir(elsewhere.path())
            .env_remove("ISUSCOPE_PROJECT");
        if let Some(project) = env {
            command.env("ISUSCOPE_PROJECT", project);
        }
        command.output().unwrap()
    };
    let project_path = project.path().to_str().unwrap();
    let flag = isuscope(&["--project", project_path, "list"], None);
    assert!(
        flag.status.success(),
        "{}",
        String::from_utf8_lossy(&flag.stderr)
    );
    let list: serde_json::Value = parsed(&flag.stdout).unwrap();
    assert_eq!(list["runs"]["rows"][0]["score"], 42);
    // 位置はsubcommandの後でもよい。
    let brief = isuscope(&["brief", "latest", "--project", project_path], None);
    assert!(brief.status.success());
    let by_env = isuscope(&["brief", "latest"], Some(project.path()));
    assert!(
        by_env.status.success(),
        "{}",
        String::from_utf8_lossy(&by_env.stderr)
    );
    let brief: serde_json::Value = parsed(&by_env.stdout).unwrap();
    assert_eq!(brief["summary"]["score"], 42);
    // 分析は書き込むので、別のdirectoryからは受け付けない。
    let analyze = isuscope(
        &["analyze", "latest", "supported", "--analysis", "x"],
        Some(project.path()),
    );
    assert!(!analyze.status.success());
    assert!(
        String::from_utf8_lossy(&analyze.stderr).contains("only for commands that read"),
        "{}",
        String::from_utf8_lossy(&analyze.stderr)
    );
    // projectの中からなら、同じprojectを指す`ISUSCOPE_PROJECT`があっても書き込める。
    let inside = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .args(["analyze", "latest", "supported", "--analysis", "x"])
        .current_dir(project.path())
        .env("ISUSCOPE_PROJECT", project.path())
        .output()
        .unwrap();
    assert!(
        inside.status.success(),
        "{}",
        String::from_utf8_lossy(&inside.stderr)
    );
}
