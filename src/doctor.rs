use crate::config::{BenchmarkMode, LoadedConfig, Transport, resolve};
use anyhow::{Context, Result};
use chrono::Utc;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;
use uuid::Uuid;

#[derive(Debug, Default)]
pub struct DoctorReport {
    pub passed: usize,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
}

impl DoctorReport {
    fn pass(&mut self, message: impl AsRef<str>) {
        self.passed += 1;
        println!("✓ {}", message.as_ref());
    }

    fn warn(&mut self, message: impl Into<String>) {
        let message = message.into();
        println!("! {message}");
        self.warnings.push(message);
    }

    fn fail(&mut self, message: impl Into<String>) {
        let message = message.into();
        println!("✗ {message}");
        self.failures.push(message);
    }

    pub fn healthy(&self) -> bool {
        self.failures.is_empty()
    }
}

pub async fn run(config: &LoadedConfig) -> Result<DoctorReport> {
    let mut report = DoctorReport::default();
    report.pass(format!("config {}", config.config_path.display()));
    check_data_dir(config, &mut report);
    check_tooling(config, &mut report).await;
    check_commands(config, &mut report);
    check_identity(config, &mut report);
    check_nodes(config, &mut report).await;
    Ok(report)
}

fn check_data_dir(config: &LoadedConfig, report: &mut DoctorReport) {
    if let Err(error) = fs::create_dir_all(&config.data_dir) {
        report.fail(format!(
            "data directory is not writable ({}): {error}",
            config.data_dir.display()
        ));
        return;
    }
    let probe = config.data_dir.join(format!(".doctor-{}", Uuid::now_v7()));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            report.pass(format!(
                "data directory writable: {}",
                config.data_dir.display()
            ));
        }
        Err(error) => report.fail(format!(
            "data directory is not writable ({}): {error}",
            config.data_dir.display()
        )),
    }
    #[cfg(unix)]
    match free_bytes(&config.data_dir) {
        Ok(bytes) if bytes < 1024 * 1024 * 1024 => report.warn(format!(
            "data filesystem has only {:.1} MiB free",
            bytes as f64 / 1024.0 / 1024.0
        )),
        Ok(bytes) => report.pass(format!(
            "data filesystem free space: {:.1} GiB",
            bytes as f64 / 1024.0 / 1024.0 / 1024.0
        )),
        Err(error) => report.warn(format!("cannot determine free disk space: {error:#}")),
    }
}

async fn check_tooling(config: &LoadedConfig, report: &mut DoctorReport) {
    let config_dir = config.config_path.parent().unwrap_or(&config.project_root);
    for relative in &config.config.tooling.include {
        let path = config_dir.join(relative);
        if !path.is_file() {
            report.fail(format!("tooling include is missing: {}", path.display()));
            continue;
        }
        if fs::File::open(&path).is_err() {
            report.fail(format!(
                "tooling include is not readable: {}",
                path.display()
            ));
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) == Some("sh") {
            let shell = fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.lines().next().map(str::to_owned))
                .filter(|line| line.contains("bash"))
                .map(|_| "bash")
                .unwrap_or("sh");
            match Command::new(shell)
                .arg("-n")
                .arg(&path)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    report.pass(format!("shell syntax: {}", path.display()))
                }
                Ok(output) => report.fail(format!(
                    "shell syntax error in {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
                Err(error) => report.fail(format!(
                    "cannot check shell syntax for {}: {error}",
                    path.display()
                )),
            }
        } else {
            report.pass(format!("tooling include readable: {}", path.display()));
        }
    }
}

fn check_commands(config: &LoadedConfig, report: &mut DoctorReport) {
    if matches!(config.config.benchmark.mode, BenchmarkMode::Command) {
        check_program(
            &config.config.benchmark.command[0],
            &config.benchmark_working_dir(),
            "benchmark",
            report,
        );
    }
    for parser in &config.config.benchmark.parsers {
        check_program(
            &parser.command[0],
            &config.project_root,
            &format!("benchmark parser `{}`", parser.name),
            report,
        );
    }
    for collector in &config.config.collectors {
        if matches!(collector.transport, Transport::Local) {
            check_program(
                &collector.command[0],
                &config.project_root,
                &format!("local collector `{}`", collector.name),
                report,
            );
        }
    }
}

fn check_program(program: &str, working_dir: &Path, label: &str, report: &mut DoctorReport) {
    if find_program(program, working_dir).is_some() {
        report.pass(format!("{label} command: {program}"));
    } else {
        report.fail(format!(
            "{label} command was not found or is not executable: {program}"
        ));
    }
}

fn find_program(program: &str, working_dir: &Path) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            working_dir.join(path)
        };
        return executable(&path).then_some(path);
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(program))
            .find(|candidate| executable(candidate))
    })
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

fn check_identity(config: &LoadedConfig, report: &mut DoctorReport) {
    let Some(identity) = &config.config.ssh.identity_file else {
        return;
    };
    let path = resolve(&config.project_root, identity);
    if path.is_file() {
        report.pass(format!("SSH identity: {}", path.display()));
    } else {
        report.fail(format!("SSH identity is missing: {}", path.display()));
    }
}

async fn check_nodes(config: &LoadedConfig, report: &mut DoctorReport) {
    if config.config.nodes.is_empty() {
        report.warn("no SSH nodes are configured");
        return;
    }
    if find_program("ssh", &config.project_root).is_none() {
        report.fail("ssh command was not found");
        return;
    }
    for node in &config.config.nodes {
        let user = node.user.as_deref().unwrap_or(&config.config.ssh.user);
        let target = format!("{user}@{}", node.host);
        let mut args = vec![
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            format!(
                "ConnectTimeout={}",
                config.config.ssh.connect_timeout_seconds
            ),
        ];
        if let Some(identity) = &config.config.ssh.identity_file {
            args.push("-i".into());
            args.push(
                resolve(&config.project_root, identity)
                    .display()
                    .to_string(),
            );
        }
        args.extend([target.clone(), "--".into(), "date".into(), "+%s".into()]);
        let result = tokio::time::timeout(
            Duration::from_secs(config.config.ssh.connect_timeout_seconds + 3),
            Command::new("ssh").args(&args).output(),
        )
        .await;
        match result {
            Ok(Ok(output)) if output.status.success() => {
                let remote = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<i64>();
                match remote {
                    Ok(remote) => {
                        let skew = (Utc::now().timestamp() - remote).abs();
                        if skew > 2 {
                            report.warn(format!("SSH {target}: clock differs by {skew}s"));
                        } else {
                            report.pass(format!("SSH {target}: connected, clock skew {skew}s"));
                        }
                        if node_requires_sudo(config, node) {
                            check_sudo(config, &target, report).await;
                        }
                    }
                    Err(_) => report.fail(format!("SSH {target}: invalid clock response")),
                }
            }
            Ok(Ok(output)) => report.fail(format!(
                "SSH {target} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Ok(Err(error)) => report.fail(format!("SSH {target} failed: {error}")),
            Err(_) => report.fail(format!("SSH {target} timed out")),
        }
    }
}

fn node_requires_sudo(config: &LoadedConfig, node: &crate::config::NodeConfig) -> bool {
    config.config.collectors.iter().any(|collector| {
        matches!(collector.transport, Transport::Ssh)
            && (collector.roles.is_empty()
                || collector.roles.iter().any(|role| node.roles.contains(role)))
            && collector.command.iter().any(|argument| {
                argument
                    .split_whitespace()
                    .any(|part| part == "sudo" || part.ends_with("/sudo"))
            })
    })
}

async fn check_sudo(config: &LoadedConfig, target: &str, report: &mut DoctorReport) {
    let mut args = vec![
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "-o".to_owned(),
        format!(
            "ConnectTimeout={}",
            config.config.ssh.connect_timeout_seconds
        ),
    ];
    if let Some(identity) = &config.config.ssh.identity_file {
        args.push("-i".into());
        args.push(
            resolve(&config.project_root, identity)
                .display()
                .to_string(),
        );
    }
    args.extend([
        target.to_owned(),
        "--".into(),
        "sudo".into(),
        "-n".into(),
        "true".into(),
    ]);
    let result = tokio::time::timeout(
        Duration::from_secs(config.config.ssh.connect_timeout_seconds + 3),
        Command::new("ssh").args(&args).output(),
    )
    .await;
    match result {
        Ok(Ok(output)) if output.status.success() => {
            report.pass(format!("SSH {target}: non-interactive sudo"));
        }
        Ok(Ok(output)) => report.fail(format!(
            "SSH {target}: non-interactive sudo failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Ok(Err(error)) => report.fail(format!("SSH {target}: sudo check failed: {error}")),
        Err(_) => report.fail(format!("SSH {target}: sudo check timed out")),
    }
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> Result<u64> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes()).context("data path contains NUL")?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is a valid NUL-terminated string and stats points to writable memory.
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: statvfs initialized stats after returning success.
    let stats = unsafe { stats.assume_init() };
    Ok(u64::from(stats.f_bavail).saturating_mul(stats.f_frsize))
}
