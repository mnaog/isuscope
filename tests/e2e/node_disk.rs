use super::*;

#[cfg(unix)]
#[test]
fn a_node_below_the_disk_minimum_stops_the_benchmark_and_fails_doctor() {
    use std::os::unix::fs::PermissionsExt;

    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    let tools = project.path().join("tools");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&tools).unwrap();
    // `df` reports $DF_AVAILABLE_KIB free on /; every other command runs locally.
    let fake_ssh = tools.join("ssh");
    fs::write(
        &fake_ssh,
        "#!/bin/sh\nfor last; do :; done\ncase \"$last\" in df\\ -Pk*) printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\n/dev/root 30000000 29000000 %s 97%% /\\n/dev/root 30000000 29000000 %s 97%% /\\n' \"$DF_AVAILABLE_KIB\" \"$DF_AVAILABLE_KIB\"; exit 0 ;; esac\nexec /bin/sh -c \"$last\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-ran; printf '%s\n' '{\"type\":\"isuscope.result\",\"pass\":true,\"score\":1}'"]

[[nodes]]
name = "app1"
host = "app1.internal"
"#,
    )
    .unwrap();
    let path = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());
    let isuscope = |args: &[&str], available_kib: &str| {
        Command::new(env!("CARGO_BIN_EXE_isuscope"))
            .args(args)
            .env("PATH", &path)
            .env("DF_AVAILABLE_KIB", available_kib)
            .current_dir(project.path())
            .output()
            .unwrap()
    };

    // 512 MiB free: below the 1024 MiB minimum.
    let blocked = isuscope(&["run", "--hypothesis", "full disk"], "524288");
    assert!(!blocked.status.success());
    assert!(!project.path().join("benchmark-ran").exists());
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(stderr.contains("app1: / has 512 MiB free"), "{stderr}");

    let doctor = isuscope(&["doctor"], "524288");
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(
        stdout.contains("disk ubuntu@app1.internal: / has 512 MiB free (below 1024 MiB)"),
        "{stdout}"
    );

    // 2 GiB free: warns but benchmarks.
    let warned = isuscope(&["run", "--hypothesis", "low disk"], "2097152");
    assert!(
        project.path().join("benchmark-ran").is_file(),
        "{}",
        String::from_utf8_lossy(&warned.stderr)
    );
    assert!(String::from_utf8_lossy(&warned.stderr).contains("! disk app1: / has 2.0 GiB free"));
}
