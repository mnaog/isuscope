use super::*;

#[test]
fn doctor_checks_without_starting_benchmark() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("check.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::write(
        config_dir.join("config.toml"),
        r#"
[tooling]
include = ["check.sh"]

[benchmark]
mode = "command"
command = ["sh", "-c", "touch benchmark-ran"]
"#,
    )
    .unwrap();
    let doctor = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("doctor")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert!(!project.path().join("benchmark-ran").exists());
    let output = String::from_utf8(doctor.stdout).unwrap();
    assert!(output.contains("failures  0"));
    assert!(output.contains("no SSH nodes are configured"));
}

#[cfg(unix)]
fn executable(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(unix)]
#[test]
fn doctor_profile_preflight_checks_real_tools_behind_shell_wrappers() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    let tools = project.path().join("tools");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&tools).unwrap();
    executable(&tools.join("sh"), "#!/bin/sh\nexec /bin/sh \"$@\"\n");
    executable(&tools.join("perf"), "#!/bin/sh\nexit 0\n");
    executable(&tools.join("stackcollapse-perf.pl"), "#!/bin/sh\nexit 0\n");
    executable(&tools.join("flamegraph.pl"), "#!/bin/sh\nexit 0\n");
    executable(&tools.join("offcputime-bpfcc"), "#!/bin/sh\nexit 0\n");
    executable(&tools.join("python3"), "#!/bin/sh\nexit 0\n");
    executable(
        &tools.join("sudo"),
        "#!/bin/sh\ntest \"${1:-}\" = -n && shift\nexec \"$@\"\n",
    );
    executable(&tools.join("env"), "#!/bin/sh\nexec /usr/bin/env \"$@\"\n");
    executable(&tools.join("true"), "#!/bin/sh\nexit 0\n");
    executable(&tools.join("uname"), "#!/bin/sh\necho test-kernel\n");
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "external"

[[collectors]]
name = "perf-flamegraph"
phase = "after"
transport = "local"
command = ["/bin/sh", "-c", "exit 75"]

[[collectors]]
name = "offcpu"
phase = "during"
transport = "local"
command = ["/bin/sh", "-c", "exit 75"]
"#,
    )
    .unwrap();
    let doctor = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("doctor")
        .env("PATH", &tools)
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        doctor.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    let output = String::from_utf8(doctor.stdout).unwrap();
    assert!(output.contains("profile `perf-flamegraph` local: ready:"));
    assert!(output.contains("profile `offcpu` local: ready:"));
    assert!(output.contains("warnings  1"));
    assert!(output.contains("failures  0"));
}

#[cfg(unix)]
#[test]
fn doctor_profile_preflight_explains_missing_nested_dependency() {
    let project = tempdir().unwrap();
    let config_dir = project.path().join(".isuscope");
    let tools = project.path().join("tools");
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&tools).unwrap();
    executable(&tools.join("sh"), "#!/bin/sh\nexec /bin/sh \"$@\"\n");
    fs::write(
        config_dir.join("config.toml"),
        r#"
[benchmark]
mode = "external"

[[collectors]]
name = "perf-flamegraph"
phase = "after"
transport = "local"
command = ["/bin/sh", "-c", "exit 75"]
"#,
    )
    .unwrap();
    let doctor = Command::new(env!("CARGO_BIN_EXE_isuscope"))
        .arg("doctor")
        .env("PATH", &tools)
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(doctor.status.success());
    let output = String::from_utf8(doctor.stdout).unwrap();
    assert!(
        output.contains("profile `perf-flamegraph` local: unavailable (missing executable: perf)")
    );
    assert!(output.contains("failures  0"));
}
