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
