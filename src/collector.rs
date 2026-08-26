use crate::{
    benchmark::compress_log,
    config::{CollectorConfig, CollectorPhase, LoadedConfig, NodeConfig, Transport},
    model::{CollectorResult, Fingerprint, LogRef, Metric, RunMode, Transition},
    process,
    shutdown::Shutdown,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::{Child, Command};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
};
use uuid::Uuid;

pub struct CollectorOutput {
    pub result: CollectorResult,
    pub logs: Vec<LogRef>,
    pub metrics: Vec<Metric>,
    pub fingerprints: Vec<Fingerprint>,
    pub transitions: Vec<Transition>,
}

pub struct RunningCollector {
    child: Child,
    spec: ExecutionSpec,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout_capture: JoinHandle<Result<bool>>,
    stderr_capture: JoinHandle<Result<bool>>,
}

pub async fn cleanup_abandoned(config: &LoadedConfig, run_ids: &[String]) {
    for run_id in run_ids {
        if Uuid::parse_str(run_id).is_err() {
            eprintln!("! refusing cleanup for invalid run ID {run_id}");
            continue;
        }
        for node in &config.config.nodes {
            if let Err(error) = cleanup_node(config, node, run_id).await {
                eprintln!("! abandoned cleanup failed on {}: {error:#}", node.name);
            }
        }
    }
}

async fn cleanup_node(config: &LoadedConfig, node: &NodeConfig, run_id: &str) -> Result<()> {
    let user = node.user.as_deref().unwrap_or(&config.config.ssh.user);
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        format!(
            "ConnectTimeout={}",
            config.config.ssh.connect_timeout_seconds
        ),
    ];
    if let Some(identity) = &config.config.ssh.identity_file {
        args.push("-i".into());
        args.push(identity.display().to_string());
    }
    args.push(format!("{user}@{}", node.host));
    args.push("--".into());
    let script = format!("find /tmp -maxdepth 1 -type f -name 'isuscope-{run_id}.*' -delete");
    args.push(
        ["sh", "-c", &script]
            .iter()
            .map(|part| shell_quote(part))
            .collect::<Vec<_>>()
            .join(" "),
    );
    let status = tokio::time::timeout(
        Duration::from_secs(config.config.ssh.connect_timeout_seconds + 5),
        Command::new("ssh").args(args).status(),
    )
    .await
    .context("abandoned cleanup timed out")??;
    if !status.success() {
        anyhow::bail!("cleanup command exited with {status}");
    }
    Ok(())
}

#[derive(Clone)]
struct ExecutionSpec {
    collector: CollectorConfig,
    node: Option<NodeConfig>,
    program: String,
    args: Vec<String>,
    id_prefix: String,
    working_dir: PathBuf,
}

pub fn selected(
    config: &LoadedConfig,
    mode: RunMode,
    phase: CollectorPhase,
) -> impl Iterator<Item = &CollectorConfig> {
    config.config.collectors.iter().filter(move |collector| {
        collector.enabled_for(mode) && matches_phase(collector.phase, phase)
    })
}

pub async fn run_phase(
    config: &LoadedConfig,
    mode: RunMode,
    phase: CollectorPhase,
    run_id: &str,
    run_dir: &Path,
    shutdown: Option<Shutdown>,
) -> Vec<CollectorOutput> {
    let mut outputs = Vec::new();
    for collector in selected(config, mode, phase) {
        match expand(config, collector, run_id, run_dir) {
            Ok(specs) if specs.is_empty() => {
                outputs.push(skipped(collector, "no node matched collector roles"))
            }
            Ok(specs) => {
                for spec in specs {
                    outputs.push(run_once(spec, run_dir, shutdown.clone()).await);
                }
            }
            Err(error) => outputs.push(failed(collector, None, format!("{error:#}"))),
        }
    }
    outputs
}

pub async fn start_during(
    config: &LoadedConfig,
    mode: RunMode,
    run_id: &str,
    run_dir: &Path,
) -> (Vec<RunningCollector>, Vec<CollectorOutput>) {
    let mut running = Vec::new();
    let mut failed_outputs = Vec::new();
    for collector in selected(config, mode, CollectorPhase::During) {
        match expand(config, collector, run_id, run_dir) {
            Ok(specs) if specs.is_empty() => {
                failed_outputs.push(skipped(collector, "no node matched collector roles"))
            }
            Ok(specs) => {
                for spec in specs {
                    match spawn(spec.clone(), run_dir) {
                        Ok(value) => running.push(value),
                        Err(error) => failed_outputs.push(failed(
                            collector,
                            spec.node.as_ref().map(|node| node.name.clone()),
                            format!("{error:#}"),
                        )),
                    }
                }
            }
            Err(error) => failed_outputs.push(failed(collector, None, format!("{error:#}"))),
        }
    }
    (running, failed_outputs)
}

pub async fn stop_during(running: Vec<RunningCollector>, run_dir: &Path) -> Vec<CollectorOutput> {
    let mut outputs = Vec::new();
    for mut collector in running {
        let already_finished = collector.child.try_wait().ok().flatten();
        let status = match already_finished {
            Some(status) => Some(status),
            None => process::terminate_group(&mut collector.child).await.ok(),
        };
        outputs.push(
            finalize(
                collector,
                status.and_then(|value| value.code()),
                None,
                true,
                run_dir,
            )
            .await,
        );
    }
    outputs
}

async fn run_once(
    spec: ExecutionSpec,
    run_dir: &Path,
    shutdown: Option<Shutdown>,
) -> CollectorOutput {
    let timeout_seconds = spec.collector.timeout_seconds;
    let mut running = match spawn(spec.clone(), run_dir) {
        Ok(value) => value,
        Err(error) => {
            return failed(
                &spec.collector,
                spec.node.as_ref().map(|node| node.name.clone()),
                format!("{error:#}"),
            );
        }
    };
    let waited = if let Some(mut shutdown) = shutdown {
        tokio::select! {
            value = tokio::time::timeout(Duration::from_secs(timeout_seconds), running.child.wait()) => value,
            _ = shutdown.cancelled() => {
                let _ = process::terminate_group(&mut running.child).await;
                return finalize(
                    running,
                    None,
                    Some("interrupted by signal".into()),
                    false,
                    run_dir,
                ).await;
            }
        }
    } else {
        tokio::time::timeout(Duration::from_secs(timeout_seconds), running.child.wait()).await
    };
    let (exit_code, error) = match waited {
        Ok(Ok(status)) => (status.code(), None),
        Ok(Err(error)) => (None, Some(error.to_string())),
        Err(_) => {
            let _ = process::terminate_group(&mut running.child).await;
            (
                None,
                Some(format!("collector timed out after {timeout_seconds}s")),
            )
        }
    };
    finalize(running, exit_code, error, false, run_dir).await
}

fn spawn(spec: ExecutionSpec, run_dir: &Path) -> Result<RunningCollector> {
    let stdout_path = run_dir
        .join("tmp")
        .join(format!("{}-stdout.log", spec.id_prefix));
    let stderr_path = run_dir
        .join("tmp")
        .join(format!("{}-stderr.log", spec.id_prefix));
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.working_dir)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process::configure_group(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start collector `{}`", spec.collector.name))?;
    let stdout = child
        .stdout
        .take()
        .context("collector stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("collector stderr was not captured")?;
    let stdout_capture = tokio::spawn(capture_capped(
        stdout,
        stdout_path.clone(),
        spec.collector.max_output_bytes,
    ));
    let stderr_capture = tokio::spawn(capture_capped(
        stderr,
        stderr_path.clone(),
        spec.collector.max_output_bytes,
    ));
    Ok(RunningCollector {
        child,
        spec,
        stdout_path,
        stderr_path,
        stdout_capture,
        stderr_capture,
    })
}

async fn finalize(
    running: RunningCollector,
    exit_code: Option<i32>,
    error: Option<String>,
    intentionally_stopped: bool,
    run_dir: &Path,
) -> CollectorOutput {
    let RunningCollector {
        spec,
        stdout_path,
        stderr_path,
        stdout_capture,
        stderr_capture,
        ..
    } = running;
    let mut logs = Vec::new();
    let stdout_id = format!("{}-stdout", spec.id_prefix);
    let stderr_id = format!("{}-stderr", spec.id_prefix);
    let stdout_destination = run_dir.join("logs").join(format!("{stdout_id}.zst"));
    let stderr_destination = run_dir.join("logs").join(format!("{stderr_id}.zst"));
    let mut compression_errors = Vec::new();
    match stdout_capture.await {
        Ok(Ok(true)) => compression_errors.push(format!(
            "stdout truncated at {} bytes",
            spec.collector.max_output_bytes
        )),
        Ok(Ok(false)) => {}
        Ok(Err(error)) => compression_errors.push(format!("stdout capture failed: {error}")),
        Err(error) => compression_errors.push(format!("stdout capture task failed: {error}")),
    }
    match stderr_capture.await {
        Ok(Ok(true)) => compression_errors.push(format!(
            "stderr truncated at {} bytes",
            spec.collector.max_output_bytes
        )),
        Ok(Ok(false)) => {}
        Ok(Err(error)) => compression_errors.push(format!("stderr capture failed: {error}")),
        Err(error) => compression_errors.push(format!("stderr capture task failed: {error}")),
    }
    if let Err(value) = compress_log(&stdout_path, &stdout_destination) {
        compression_errors.push(value.to_string());
    } else {
        logs.push(LogRef {
            id: stdout_id.clone(),
            kind: format!("collector:{}:stdout", spec.collector.name),
            node: spec.node.as_ref().map(|node| node.name.clone()),
        });
    }
    if let Err(value) = compress_log(&stderr_path, &stderr_destination) {
        compression_errors.push(value.to_string());
    } else {
        logs.push(LogRef {
            id: stderr_id.clone(),
            kind: format!("collector:{}:stderr", spec.collector.name),
            node: spec.node.as_ref().map(|node| node.name.clone()),
        });
    }
    let (mut metrics, mut fingerprints, transitions) =
        parse_protocol(&stdout_destination).unwrap_or_default();
    if let Some(node) = &spec.node {
        for metric in &mut metrics {
            metric
                .labels
                .entry("node".into())
                .or_insert_with(|| node.name.clone());
        }
        for fingerprint in &mut fingerprints {
            fingerprint
                .labels
                .entry("node".into())
                .or_insert_with(|| node.name.clone());
        }
    }
    let error = match (error, compression_errors.is_empty()) {
        (Some(error), true) => Some(error),
        (Some(error), false) => Some(format!("{error}; {}", compression_errors.join("; "))),
        (None, false) => Some(compression_errors.join("; ")),
        (None, true) => None,
    };
    let success = error.is_none() && (intentionally_stopped || exit_code == Some(0));
    CollectorOutput {
        result: CollectorResult {
            name: spec.collector.name,
            node: spec.node.map(|node| node.name),
            phase: spec.collector.phase.as_str().into(),
            status: if success { "complete" } else { "failed" }.into(),
            exit_code,
            error,
            log_ids: logs.iter().map(|log| log.id.clone()).collect(),
        },
        logs,
        metrics,
        fingerprints,
        transitions,
    }
}

async fn capture_capped<R>(mut reader: R, path: PathBuf, limit: u64) -> Result<bool>
where
    R: AsyncRead + Unpin,
{
    let mut file = tokio::fs::File::create(path).await?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut written = 0_u64;
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(written) as usize;
        let accepted = remaining.min(read);
        if accepted > 0 {
            file.write_all(&buffer[..accepted]).await?;
            written += accepted as u64;
        }
        if accepted < read {
            truncated = true;
        }
    }
    file.flush().await?;
    Ok(truncated)
}

pub(crate) fn parse_protocol(
    path: &Path,
) -> Result<(Vec<Metric>, Vec<Fingerprint>, Vec<Transition>)> {
    let input = fs::File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(input)?;
    let reader = std::io::BufReader::new(decoder);
    let mut metrics = Vec::new();
    let mut fingerprints = Vec::new();
    let mut transitions = Vec::new();
    use std::io::BufRead;
    for line in reader.lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("metric") => {
                let Some(name) = value.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let Some(number) = value.get("value").and_then(Value::as_f64) else {
                    continue;
                };
                let labels = value
                    .get("labels")
                    .and_then(Value::as_object)
                    .map(|labels| {
                        labels
                            .iter()
                            .filter_map(|(key, value)| {
                                value.as_str().map(|value| (key.clone(), value.to_owned()))
                            })
                            .collect()
                    })
                    .unwrap_or_else(BTreeMap::new);
                metrics.push(Metric {
                    name: name.to_owned(),
                    value: number,
                    unit: value
                        .get("unit")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    labels,
                });
            }
            Some("transition") => {
                let Some(from_route) = value.get("from").and_then(Value::as_str) else {
                    continue;
                };
                let Some(to_route) = value.get("to").and_then(Value::as_str) else {
                    continue;
                };
                transitions.push(Transition {
                    from_route: from_route.to_owned(),
                    to_route: to_route.to_owned(),
                    count: value.get("count").and_then(Value::as_i64).unwrap_or(1),
                    p50_ms: value.get("p50_ms").and_then(Value::as_f64),
                    p95_ms: value.get("p95_ms").and_then(Value::as_f64),
                });
            }
            Some("fingerprint") => {
                let Some(name) = value.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let Some(fingerprint_value) = value.get("value").and_then(Value::as_str) else {
                    continue;
                };
                let labels = value
                    .get("labels")
                    .and_then(Value::as_object)
                    .map(|labels| {
                        labels
                            .iter()
                            .filter_map(|(key, value)| {
                                value.as_str().map(|value| (key.clone(), value.to_owned()))
                            })
                            .collect()
                    })
                    .unwrap_or_else(BTreeMap::new);
                fingerprints.push(Fingerprint {
                    name: name.to_owned(),
                    value: fingerprint_value.to_owned(),
                    labels,
                });
            }
            _ => {}
        }
    }
    Ok((metrics, fingerprints, transitions))
}

fn expand(
    config: &LoadedConfig,
    collector: &CollectorConfig,
    run_id: &str,
    run_dir: &Path,
) -> Result<Vec<ExecutionSpec>> {
    match collector.transport {
        Transport::Local => Ok(vec![make_spec(config, collector, None, run_id, run_dir)?]),
        Transport::Ssh => {
            let nodes = config.config.nodes.iter().filter(|node| {
                collector.roles.is_empty()
                    || collector.roles.iter().any(|role| node.roles.contains(role))
            });
            nodes
                .map(|node| make_spec(config, collector, Some(node), run_id, run_dir))
                .collect()
        }
    }
}

fn make_spec(
    config: &LoadedConfig,
    collector: &CollectorConfig,
    node: Option<&NodeConfig>,
    run_id: &str,
    run_dir: &Path,
) -> Result<ExecutionSpec> {
    let expanded = collector
        .command
        .iter()
        .map(|argument| replace_placeholders(argument, run_id, run_dir, node))
        .collect::<Vec<_>>();
    let (program, args) = expanded
        .split_first()
        .context("collector.command must not be empty")?;
    let node_name = node.map(|node| node.name.as_str()).unwrap_or("local");
    let id_prefix = format!(
        "{}-{}-{}",
        sanitize(&collector.name),
        sanitize(node_name),
        collector.phase.as_str(),
    );
    if matches!(collector.transport, Transport::Ssh) {
        let node = node.context("SSH collector has no target node")?;
        let user = node.user.as_deref().unwrap_or(&config.config.ssh.user);
        let mut ssh_args = vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            format!(
                "ConnectTimeout={}",
                config.config.ssh.connect_timeout_seconds
            ),
        ];
        if let Some(identity) = &config.config.ssh.identity_file {
            ssh_args.push("-i".into());
            ssh_args.push(identity.display().to_string());
        }
        ssh_args.push(format!("{user}@{}", node.host));
        ssh_args.push("--".into());
        ssh_args.push(
            expanded
                .iter()
                .map(|part| shell_quote(part))
                .collect::<Vec<_>>()
                .join(" "),
        );
        return Ok(ExecutionSpec {
            collector: collector.clone(),
            node: Some(node.clone()),
            program: "ssh".into(),
            args: ssh_args,
            id_prefix,
            working_dir: config.project_root.clone(),
        });
    }
    Ok(ExecutionSpec {
        collector: collector.clone(),
        node: node.cloned(),
        program: program.clone(),
        args: args.to_vec(),
        id_prefix,
        working_dir: config.project_root.clone(),
    })
}

fn replace_placeholders(
    value: &str,
    run_id: &str,
    run_dir: &Path,
    node: Option<&NodeConfig>,
) -> String {
    value
        .replace("{run_id}", run_id)
        .replace("{run_dir}", &run_dir.display().to_string())
        .replace(
            "{node}",
            node.map(|node| node.name.as_str()).unwrap_or("local"),
        )
        .replace(
            "{host}",
            node.map(|node| node.host.as_str()).unwrap_or("localhost"),
        )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn failed(collector: &CollectorConfig, node: Option<String>, error: String) -> CollectorOutput {
    CollectorOutput {
        result: CollectorResult {
            name: collector.name.clone(),
            node,
            phase: collector.phase.as_str().into(),
            status: "failed".into(),
            exit_code: None,
            error: Some(error),
            log_ids: Vec::new(),
        },
        logs: Vec::new(),
        metrics: Vec::new(),
        fingerprints: Vec::new(),
        transitions: Vec::new(),
    }
}

fn skipped(collector: &CollectorConfig, reason: &str) -> CollectorOutput {
    let mut output = failed(collector, None, reason.to_owned());
    output.result.status = "skipped".into();
    output
}

fn matches_phase(left: CollectorPhase, right: CollectorPhase) -> bool {
    std::mem::discriminant(&left) == std::mem::discriminant(&right)
}
