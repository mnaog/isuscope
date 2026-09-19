use crate::{
    benchmark::compress_log,
    config::{
        CollectorConfig, CollectorParser, CollectorPhase, LoadedConfig, NodeConfig, Transport,
    },
    model::{CollectorResult, Fingerprint, LogRef, Metric, RunMode, Transition},
    process,
    shutdown::Shutdown,
    transition::RouteNormalizer,
};
use anyhow::{Context, Result};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
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
        // ベンチ機などルール側のnodeには、collectorと同じく一切触れない。
        for node in config.config.nodes.iter().filter(|node| !node.rule_side) {
            if let Err(error) = cleanup_node(config, node, run_id).await {
                eprintln!("! abandoned cleanup failed on {}: {error:#}", node.name);
            }
        }
    }
}

async fn cleanup_node(config: &LoadedConfig, node: &NodeConfig, run_id: &str) -> Result<()> {
    let user = node.user.as_deref().unwrap_or(&config.config.ssh.user);
    let mut args = config.ssh_options();
    args.push(format!("{user}@{}", node.host));
    args.push("--".into());
    let script = format!(
        "base=/tmp/isuscope-{run_id}.perf; if sudo -n test -s \"$base.pid\" 2>/dev/null; then pid=$(sudo -n cat \"$base.pid\" 2>/dev/null || true); case $pid in ''|*[!0-9]*) ;; *) sudo -n kill -INT \"$pid\" 2>/dev/null || true ;; esac; fi; offcpu=/tmp/isuscope-{run_id}.offcpu; if sudo -n test -s \"$offcpu.pid\" 2>/dev/null; then pid=$(sudo -n cat \"$offcpu.pid\" 2>/dev/null || true); case $pid in ''|*[!0-9]*) ;; *) sudo -n kill -INT -- \"-$pid\" 2>/dev/null || true ;; esac; fi; sudo -n rm -f \"$base.data\" \"$base.log\" \"$base.pid\" \"$offcpu.pid\" \"$offcpu.out\" \"$offcpu.err\" 2>/dev/null || true; find /tmp -maxdepth 1 -type f -name 'isuscope-{run_id}.*' -delete"
    );
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

/// Runs a phase with one queue per node, and the queues in parallel. Collectors on the same
/// node keep their configured order, because they hand work to each other there (perf-stop
/// before perf-report, a log mark before its delta). Local collectors run last and in order,
/// since they read what the node collectors just brought back.
pub async fn run_phase(
    config: &LoadedConfig,
    mode: RunMode,
    phase: CollectorPhase,
    run_id: &str,
    run_dir: &Path,
    shutdown: Option<Shutdown>,
) -> Vec<CollectorOutput> {
    let mut outputs = Vec::new();
    let mut queues: Vec<(String, Vec<(usize, ExecutionSpec)>)> = Vec::new();
    let mut local: Vec<(usize, ExecutionSpec)> = Vec::new();
    let mut order = 0;
    for collector in selected(config, mode, phase) {
        match expand(config, collector, run_id, run_dir) {
            Ok(specs) if specs.is_empty() => {
                outputs.push(skipped(collector, "no node matched collector roles"))
            }
            Ok(specs) => {
                for spec in specs {
                    order += 1;
                    match &spec.node {
                        None => local.push((order, spec)),
                        Some(node) => {
                            let name = node.name.clone();
                            match queues.iter_mut().find(|(queue, _)| *queue == name) {
                                Some((_, queue)) => queue.push((order, spec)),
                                None => queues.push((name, vec![(order, spec)])),
                            }
                        }
                    }
                }
            }
            Err(error) => outputs.push(failed(collector, None, format!("{error:#}"))),
        }
    }

    let mut running = tokio::task::JoinSet::new();
    for (_, queue) in queues {
        let run_dir = run_dir.to_path_buf();
        let shutdown = shutdown.clone();
        running.spawn(async move {
            let mut results = Vec::with_capacity(queue.len());
            for (order, spec) in queue {
                results.push((order, run_once(spec, &run_dir, shutdown.clone()).await));
            }
            results
        });
    }
    let mut ordered = running
        .join_all()
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    for (order, spec) in local {
        ordered.push((order, run_once(spec, run_dir, shutdown.clone()).await));
    }
    ordered.sort_by_key(|(order, _)| *order);
    outputs.extend(ordered.into_iter().map(|(_, output)| output));
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
        let (status, intentionally_stopped) = match already_finished {
            Some(status) => (Some(status), false),
            None => (
                process::terminate_group(&mut collector.child).await.ok(),
                true,
            ),
        };
        outputs.push(
            finalize(
                collector,
                status.and_then(|value| value.code()),
                None,
                intentionally_stopped,
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
    if let Some(parser) = spec.collector.parser {
        let routes = spec.working_dir.join(".isuscope/routes.toml");
        match parse_standard_output(
            &stdout_destination,
            parser,
            routes.is_file().then_some(routes.as_path()),
            benchmark_interval(run_dir),
        ) {
            Ok(parsed) => metrics.extend(parsed),
            Err(error) => compression_errors.push(format!("standard output parse failed: {error}")),
        }
    }
    if error.is_none()
        && (intentionally_stopped || exit_code == Some(0))
        && let Err(validation_error) =
            validate_profile_artifact(&spec.collector.name, &stdout_destination)
    {
        compression_errors.push(validation_error.to_string());
    }
    for metric in &mut metrics {
        normalize_perf_labels(metric);
        metric
            .labels
            .entry("collector".into())
            .or_insert_with(|| spec.collector.name.clone());
    }
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
    let unavailable = error.is_none()
        && exit_code.is_some_and(|code| spec.collector.unavailable_exit_codes.contains(&code));
    let success = error.is_none() && (intentionally_stopped || exit_code == Some(0));
    CollectorOutput {
        result: CollectorResult {
            name: spec.collector.name,
            node: spec.node.map(|node| node.name),
            phase: spec.collector.phase.as_str().into(),
            status: if unavailable {
                "unavailable"
            } else if success {
                "complete"
            } else {
                "failed"
            }
            .into(),
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

fn validate_profile_artifact(name: &str, path: &Path) -> Result<()> {
    if !matches!(name, "perf-flamegraph" | "offcpu") {
        return Ok(());
    }
    let input = fs::File::open(path)?;
    let mut decoder = zstd::stream::read::Decoder::new(input)?;
    let mut value = String::new();
    use std::io::Read;
    decoder.read_to_string(&mut value)?;
    if name == "perf-flamegraph" {
        let trimmed = value.trim();
        if !trimmed.contains("<svg") || !trimmed.ends_with("</svg>") {
            anyhow::bail!("perf-flamegraph output is not a complete SVG document");
        }
    } else {
        if value.lines().all(|line| line.trim().is_empty()) {
            anyhow::bail!("offcpu output has no samples");
        }
        for line in value.lines().filter(|line| !line.trim().is_empty()) {
            let Some((stack, count)) = line.rsplit_once(' ') else {
                anyhow::bail!("offcpu output is not folded stack format");
            };
            if stack.is_empty() || count.parse::<u64>().is_err() {
                anyhow::bail!("offcpu output is not folded stack format");
            }
        }
    }
    Ok(())
}

pub(crate) async fn capture_capped<R>(mut reader: R, path: PathBuf, limit: u64) -> Result<bool>
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
    // Split on bytes so one line that is not UTF-8 is skipped instead of ending the parse.
    for line in reader.split(b'\n').map_while(Result::ok) {
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
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
                    timestamp: parse_metric_timestamp(value.get("timestamp")),
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

fn parse_metric_timestamp(value: Option<&Value>) -> Option<chrono::DateTime<chrono::Utc>> {
    match value? {
        Value::String(value) => chrono::DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|value| value.with_timezone(&chrono::Utc)),
        Value::Number(value) => {
            let seconds = value.as_f64()?;
            let whole = seconds.floor() as i64;
            let nanos = ((seconds - whole as f64) * 1_000_000_000.0).round() as u32;
            chrono::DateTime::from_timestamp(whole, nanos)
        }
        _ => None,
    }
}

pub(crate) fn parse_standard_output(
    path: &Path,
    parser: CollectorParser,
    routes: Option<&Path>,
    interval: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
) -> Result<Vec<Metric>> {
    let input = fs::File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(input)?;
    if matches!(parser, CollectorParser::MysqlSlow) {
        return parse_mysql_slow_reader(std::io::BufReader::new(decoder), interval);
    }
    let mut decoder = decoder;
    use std::io::Read;
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes)?;
    if matches!(parser, CollectorParser::SlpWindows) {
        // SQLの文字列literalに任意のbyteが入り得るので、1行が壊れていても全体は読む。
        return Ok(parse_slp_windows(&String::from_utf8_lossy(&bytes)));
    }
    let raw = String::from_utf8(bytes).context("stream did not contain valid UTF-8")?;
    match parser {
        CollectorParser::AlpJson => parse_alp_json(&raw, routes),
        CollectorParser::AlpWindows => parse_alp_windows(&raw, routes),
        CollectorParser::MysqlSlow => unreachable!("handled by streaming parser"),
        CollectorParser::SlpJson => parse_slp_json(&raw),
        CollectorParser::SlpTsv => parse_slp_tsv(&raw),
        CollectorParser::SlpWindows => unreachable!("handled before UTF-8 validation"),
        CollectorParser::Sysstat => Ok(parse_sysstat(&raw, interval)),
        CollectorParser::ServiceCgroup => Ok(parse_service_cgroup(&raw, interval)),
        CollectorParser::PerfScript => parse_perf_script(&raw),
    }
}

fn parse_perf_script(raw: &str) -> Result<Vec<Metric>> {
    // `# isuscope-perf-clock <wall> <uptime>`: sampleの時刻はCLOCK_MONOTONIC（uptimeと同じ基準）で、
    // 壁時計との差を足せば絶対時刻になる。`# isuscope-perf-start <wall>`は旧形式で、
    // `--reltime`の時刻（最初のsampleからの経過）をperf起動時の壁時計へ足す。
    let clock = raw
        .lines()
        .find_map(|line| line.strip_prefix("# isuscope-perf-clock "))
        .and_then(|value| {
            let mut fields = value.split_whitespace();
            let wall = fields.next()?.parse::<f64>().ok()?;
            let uptime = fields.next()?.parse::<f64>().ok()?;
            Some((wall, uptime))
        });
    let start = match clock {
        Some((wall, uptime)) => wall - uptime,
        None => raw
            .lines()
            .find_map(|line| line.strip_prefix("# isuscope-perf-start "))
            .and_then(|value| value.trim().parse::<f64>().ok())
            .context("perf script output has no valid isuscope start marker")?,
    };
    let marker_wall = clock.map(|(wall, _)| wall);
    // A sample header is `comm [pid] [cpu] time: event: [ip sym (dso)]`; perf may right-align
    // comm with leading spaces. With `perf record -g` and no `-G`, the stack follows on
    // tab-indented lines and the first frame is the leaf.
    let header_pattern =
        regex::Regex::new(r"^(?P<prefix>\S.*?)\s+(?P<time>[0-9]+[.][0-9]+):\s*(?P<rest>.*)$")?;
    let symbol_pattern = regex::Regex::new(r"(?P<symbol>.+?)\s+\((?P<dso>[^()]*)\)\s*$")?;

    struct Sample {
        at: chrono::DateTime<Utc>,
        process: String,
        leaf: Option<(String, String)>,
    }
    fn symbol_and_binary(symbol_pattern: &regex::Regex, text: &str) -> Option<(String, String)> {
        let capture = symbol_pattern.captures(text)?;
        let binary = capture
            .name("dso")
            .map_or("-", |value| value.as_str().trim());
        let raw_symbol = capture
            .name("symbol")
            .map_or("-", |value| value.as_str().trim());
        let mut fields = raw_symbol.splitn(2, char::is_whitespace);
        let first = fields.next().unwrap_or("-");
        let symbol = if !first.is_empty() && first.chars().all(|c| c.is_ascii_hexdigit()) {
            fields.next().unwrap_or("-").trim()
        } else {
            raw_symbol
        };
        Some((canonical_perf_binary(binary), canonical_perf_symbol(symbol)))
    }

    let mut buckets = BTreeMap::<(chrono::DateTime<Utc>, String, String, String), u64>::new();
    let mut parsed_lines = 0_u64;
    let mut record = |sample: Sample, buckets: &mut BTreeMap<_, u64>| {
        let Some(bucket) = chrono::DateTime::from_timestamp(sample.at.timestamp() / 5 * 5, 0)
        else {
            return;
        };
        let (binary, symbol) = sample
            .leaf
            .unwrap_or_else(|| ("[unknown]".into(), "[unknown]".into()));
        *buckets
            .entry((bucket, sample.process, binary, symbol))
            .or_default() += 1;
        parsed_lines += 1;
    };
    let mut pending: Option<Sample> = None;
    let mut saw_sample_text = false;
    for line in raw.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        saw_sample_text = true;
        if line.starts_with('\t') {
            if let Some(sample) = pending.as_mut()
                && sample.leaf.is_none()
            {
                sample.leaf = symbol_and_binary(&symbol_pattern, line.trim());
            }
            continue;
        }
        let Some(header) = header_pattern.captures(line.trim_start()) else {
            continue;
        };
        if let Some(sample) = pending.take() {
            record(sample, &mut buckets);
        }
        let relative = header["time"].parse::<f64>()?;
        let wall = start + relative;
        // 時計の基準が食い違っていれば、黙って別の時間帯へ置かずに失敗させる。
        if let Some(marker) = marker_wall
            && !(marker - 3_600.0..=marker + 86_400.0).contains(&wall)
        {
            anyhow::bail!(
                "perf sample time {wall:.3} does not match the recorded clock (started at {marker:.3})"
            );
        }
        let seconds = wall.floor() as i64;
        let nanos = ((wall - wall.floor()) * 1_000_000_000.0).round() as u32;
        let Some(at) = chrono::DateTime::from_timestamp(seconds, nanos.min(999_999_999)) else {
            continue;
        };
        let process = header["prefix"]
            .split_whitespace()
            .take_while(|part| {
                !(part.chars().all(|character| character.is_ascii_digit())
                    || part.starts_with('[') && part.ends_with(']'))
            })
            .collect::<Vec<_>>()
            .join(" ");
        // Without a call graph the leaf is on the header itself, after `event: ip`.
        let inline = header["rest"]
            .split_once(": ")
            .and_then(|(_, sample)| symbol_and_binary(&symbol_pattern, sample.trim()));
        pending = Some(Sample {
            at,
            process: if process.is_empty() {
                "-".into()
            } else {
                process
            },
            leaf: inline,
        });
    }
    if let Some(sample) = pending.take() {
        record(sample, &mut buckets);
    }
    if saw_sample_text && parsed_lines == 0 {
        anyhow::bail!("perf script output contained samples but none matched the supported format");
    }
    let mut totals = BTreeMap::<chrono::DateTime<Utc>, u64>::new();
    let mut process_buckets = BTreeMap::<(chrono::DateTime<Utc>, String), u64>::new();
    let mut process_totals = BTreeMap::<String, u64>::new();
    for ((at, process, _, _), count) in &buckets {
        *totals.entry(*at).or_default() += count;
        *process_buckets.entry((*at, process.clone())).or_default() += count;
        *process_totals.entry(process.clone()).or_default() += count;
    }
    let all_samples = process_totals.values().sum::<u64>();
    // run全体のsymbol別の割合（timestampなし）。`perf report --no-children --sort comm,dso,symbol`
    // の自己時間と同じ切り方で、perf.dataをもう一度読まずにここで作る。
    let mut symbol_totals = BTreeMap::<(String, String, String), u64>::new();
    for ((_, process, binary, symbol), count) in &buckets {
        *symbol_totals
            .entry((process.clone(), binary.clone(), symbol.clone()))
            .or_default() += count;
    }
    let symbol_metrics = symbol_totals
        .into_iter()
        .flat_map(|((process, binary, symbol), count)| {
            let labels = BTreeMap::from([
                ("process".into(), process),
                ("binary".into(), binary),
                ("symbol".into(), symbol),
            ]);
            [
                Metric {
                    name: "cpu.sample_count".into(),
                    value: count as f64,
                    unit: "samples".into(),
                    timestamp: None,
                    labels: labels.clone(),
                },
                Metric {
                    name: "cpu.sample_percent".into(),
                    value: count as f64 / all_samples as f64 * 100.0,
                    unit: "percent".into(),
                    timestamp: None,
                    labels,
                },
            ]
        })
        .collect::<Vec<_>>();
    // Per-symbol rows are too fine to show which process used the CPU, so also report the
    // process share per bucket and for the whole capture (the latter has no timestamp).
    let process_metrics = process_buckets
        .into_iter()
        .map(|((timestamp, process), count)| Metric {
            name: "cpu.process_percent".into(),
            value: count as f64 / totals[&timestamp] as f64 * 100.0,
            unit: "percent".into(),
            timestamp: Some(timestamp),
            labels: BTreeMap::from([("process".into(), process)]),
        })
        .chain(process_totals.into_iter().map(|(process, count)| Metric {
            name: "cpu.process_percent".into(),
            value: count as f64 / all_samples as f64 * 100.0,
            unit: "percent".into(),
            timestamp: None,
            labels: BTreeMap::from([("process".into(), process)]),
        }))
        .collect::<Vec<_>>();
    Ok(buckets
        .into_iter()
        .flat_map(|((timestamp, process, binary, symbol), count)| {
            let labels = BTreeMap::from([
                ("process".into(), process),
                ("binary".into(), binary),
                ("symbol".into(), symbol),
            ]);
            let percent = count as f64 / totals[&timestamp] as f64 * 100.0;
            [
                Metric {
                    name: "cpu.sample_count".into(),
                    value: count as f64,
                    unit: "samples".into(),
                    timestamp: Some(timestamp),
                    labels: labels.clone(),
                },
                Metric {
                    name: "cpu.sample_percent".into(),
                    value: percent,
                    unit: "percent".into(),
                    timestamp: Some(timestamp),
                    labels,
                },
            ]
        })
        .chain(symbol_metrics)
        .chain(process_metrics)
        .collect())
}

fn benchmark_interval(run_dir: &Path) -> Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
    let manifest: crate::model::RunManifest =
        serde_json::from_slice(&fs::read(run_dir.join("run.json")).ok()?).ok()?;
    Some((
        manifest.benchmark.started_at?,
        manifest.benchmark.finished_at?,
    ))
}

#[cfg(test)]
fn parse_mysql_slow_series(
    raw: &str,
    interval: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
) -> Vec<Metric> {
    parse_mysql_slow_reader(std::io::Cursor::new(raw.as_bytes()), interval).unwrap_or_default()
}

fn parse_mysql_slow_reader<R: std::io::BufRead>(
    reader: R,
    interval: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
) -> Result<Vec<Metric>> {
    #[derive(Default)]
    struct Event {
        timestamp: Option<chrono::DateTime<Utc>>,
        duration_ms: Option<f64>,
        lock_ms: f64,
        rows_sent: f64,
        rows_examined: f64,
        query: Vec<String>,
    }
    #[derive(Default)]
    struct Stats {
        calls: u64,
        total_ms: f64,
        lock_ms: f64,
        rows_sent: f64,
        rows_examined: f64,
        durations_ms: Vec<f64>,
    }
    fn flush(
        event: &mut Event,
        interval: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
        buckets: &mut BTreeMap<(chrono::DateTime<Utc>, String), Stats>,
        aggregate: &mut BTreeMap<String, Stats>,
    ) {
        let (Some(at), Some(duration_ms)) = (event.timestamp, event.duration_ms) else {
            *event = Event::default();
            return;
        };
        if interval.is_some_and(|(start, end)| at < start || at > end) {
            *event = Event::default();
            return;
        }
        let query = event
            .query
            .iter()
            .map(String::as_str)
            .filter(|line| {
                let line = line.trim_start();
                !line.starts_with("SET timestamp=")
                    && !line.starts_with("use ")
                    && !line.starts_with("# administrator command:")
            })
            .collect::<Vec<_>>()
            .join(" ");
        let digest = normalize_sql_digest(&query);
        if digest.is_empty() {
            *event = Event::default();
            return;
        }
        if let Some(bucket) = chrono::DateTime::from_timestamp(at.timestamp() / 5 * 5, 0) {
            let stats = buckets.entry((bucket, digest.clone())).or_default();
            stats.calls += 1;
            stats.total_ms += duration_ms;
            stats.lock_ms += event.lock_ms;
            stats.rows_sent += event.rows_sent;
            stats.rows_examined += event.rows_examined;
        }
        let stats = aggregate.entry(digest).or_default();
        stats.calls += 1;
        stats.total_ms += duration_ms;
        stats.lock_ms += event.lock_ms;
        stats.rows_sent += event.rows_sent;
        stats.rows_examined += event.rows_examined;
        stats.durations_ms.push(duration_ms);
        *event = Event::default();
    }

    let mut event = Event::default();
    let mut buckets = BTreeMap::<(chrono::DateTime<Utc>, String), Stats>::new();
    let mut aggregate = BTreeMap::<String, Stats>::new();
    for raw_line in reader.split(b'\n') {
        let raw_line = raw_line?;
        // Slow logs can include binary SQL literals. Lossy conversion is
        // intentionally scoped to one line, keeping peak memory bounded.
        let line = String::from_utf8_lossy(&raw_line);
        if let Some(value) = line.strip_prefix("# Time: ") {
            flush(&mut event, interval, &mut buckets, &mut aggregate);
            event.timestamp = chrono::DateTime::parse_from_rfc3339(value.trim())
                .ok()
                .map(|value| value.with_timezone(&Utc));
        } else if let Some(rest) = line.strip_prefix("# Query_time: ") {
            let fields = rest.split_whitespace().collect::<Vec<_>>();
            event.duration_ms = fields
                .first()
                .and_then(|value| value.parse::<f64>().ok())
                .map(|seconds| seconds * 1_000.0);
            for pair in fields.windows(2) {
                match pair[0] {
                    "Lock_time:" => {
                        event.lock_ms = pair[1].parse::<f64>().unwrap_or_default() * 1_000.0
                    }
                    "Rows_sent:" => event.rows_sent = pair[1].parse().unwrap_or_default(),
                    "Rows_examined:" => event.rows_examined = pair[1].parse().unwrap_or_default(),
                    _ => {}
                }
            }
        } else if event.duration_ms.is_some() && !line.starts_with("# User@Host:") {
            event.query.push(line.into_owned());
        }
    }
    flush(&mut event, interval, &mut buckets, &mut aggregate);
    let mut metrics = Vec::new();
    for ((timestamp, digest), stats) in buckets {
        let labels = BTreeMap::from([("engine".into(), "mysql".into()), ("digest".into(), digest)]);
        metrics.extend([
            Metric {
                name: "db.query.calls".into(),
                value: stats.calls as f64,
                unit: "queries".into(),
                timestamp: Some(timestamp),
                labels: labels.clone(),
            },
            Metric {
                name: "db.query.total_duration".into(),
                value: stats.total_ms,
                unit: "ms".into(),
                timestamp: Some(timestamp),
                labels: labels.clone(),
            },
            Metric {
                name: "db.query.lock_duration".into(),
                value: stats.lock_ms,
                unit: "ms".into(),
                timestamp: Some(timestamp),
                labels: labels.clone(),
            },
            Metric {
                name: "db.query.rows_sent".into(),
                value: stats.rows_sent,
                unit: "rows".into(),
                timestamp: Some(timestamp),
                labels: labels.clone(),
            },
            Metric {
                name: "db.query.rows_examined".into(),
                value: stats.rows_examined,
                unit: "rows".into(),
                timestamp: Some(timestamp),
                labels,
            },
        ]);
    }
    for (digest, mut stats) in aggregate {
        let labels = BTreeMap::from([("engine".into(), "mysql".into()), ("digest".into(), digest)]);
        stats.durations_ms.sort_by(f64::total_cmp);
        let p95 = percentile_value(&stats.durations_ms, 0.95);
        metrics.extend([
            Metric {
                name: "db.query.calls".into(),
                value: stats.calls as f64,
                unit: "queries".into(),
                timestamp: None,
                labels: labels.clone(),
            },
            Metric {
                name: "db.query.total_duration".into(),
                value: stats.total_ms,
                unit: "ms".into(),
                timestamp: None,
                labels: labels.clone(),
            },
            Metric {
                name: "db.query.lock_duration".into(),
                value: stats.lock_ms,
                unit: "ms".into(),
                timestamp: None,
                labels: labels.clone(),
            },
            Metric {
                name: "db.query.rows_sent".into(),
                value: stats.rows_sent,
                unit: "rows".into(),
                timestamp: None,
                labels: labels.clone(),
            },
            Metric {
                name: "db.query.rows_examined".into(),
                value: stats.rows_examined,
                unit: "rows".into(),
                timestamp: None,
                labels: labels.clone(),
            },
        ]);
        if let Some(p95) = p95 {
            metrics.push(Metric {
                name: "db.query.p95_duration".into(),
                value: p95,
                unit: "ms".into(),
                timestamp: None,
                labels,
            });
        }
    }
    Ok(metrics)
}

pub(crate) fn canonical_perf_binary(value: &str) -> String {
    let value = value.trim();
    if value.starts_with('[') && value.ends_with(']') {
        return value.to_owned();
    }
    std::path::Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .to_owned()
}

pub(crate) fn canonical_perf_symbol(value: &str) -> String {
    let mut value = value.trim();
    for prefix in ["[.] ", "[k] ", "[u] "] {
        if let Some(stripped) = value.strip_prefix(prefix) {
            value = stripped.trim();
            break;
        }
    }
    value.to_owned()
}

fn normalize_perf_labels(metric: &mut Metric) {
    if !matches!(
        metric.name.as_str(),
        "cpu.sample_count" | "cpu.sample_percent"
    ) {
        return;
    }
    if let Some(binary) = metric.labels.get_mut("binary") {
        *binary = canonical_perf_binary(binary);
    }
    if let Some(symbol) = metric.labels.get_mut("symbol") {
        *symbol = canonical_perf_symbol(symbol);
    }
}

fn percentile_value(sorted: &[f64], quantile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (quantile * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.saturating_sub(1)).copied()
}

fn normalize_sql_digest(query: &str) -> String {
    let mut output = String::new();
    let mut chars = query.chars().peekable();
    let mut pending_space = false;
    while let Some(character) = chars.next() {
        if character.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space && !output.ends_with('(') && !output.ends_with(',') {
            output.push(' ');
        }
        pending_space = false;
        if matches!(character, '\'' | '"') {
            let quote = character;
            let mut escaped = false;
            for next in chars.by_ref() {
                if escaped {
                    escaped = false;
                } else if next == '\\' {
                    escaped = true;
                } else if next == quote {
                    break;
                }
            }
            output.push('?');
        } else if character.is_ascii_digit()
            && !output.ends_with(|previous: char| {
                previous.is_ascii_alphanumeric() || matches!(previous, '_' | '$')
            })
        {
            // 識別子の途中の数字（`t1`、`user_items_2`）は値ではない。置き換えると
            // 別のtableや列が同じdigestへ混ざる。
            while chars
                .peek()
                .is_some_and(|next| next.is_ascii_alphanumeric() || matches!(next, '.' | 'x' | 'X'))
            {
                chars.next();
            }
            output.push('?');
        } else {
            output.extend(character.to_lowercase());
        }
        if output.len() >= 512 {
            output.truncate(512);
            break;
        }
    }
    output.trim().trim_end_matches(';').trim().to_owned()
}

fn json_records(raw: &str) -> Result<Vec<Value>> {
    let value: Value = serde_json::from_str(raw).context("output is not valid JSON")?;
    Ok(match value {
        Value::Array(values) => values,
        Value::Object(mut object) => object
            .remove("data")
            .or_else(|| object.remove("results"))
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_else(|| vec![Value::Object(object)]),
        _ => Vec::new(),
    })
}

fn number(object: &serde_json::Map<String, Value>, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        object.get(*name).and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
    })
}

fn string(object: &serde_json::Map<String, Value>, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str).map(str::to_owned))
}

/// `window \t <alp JSON>`。`whole`は差分全体のroute別集計、数字は5秒bucketの開始（epoch秒）で、
/// bucketからは`isuscope series`が使う回数・error・分位点だけを時系列として残す。
/// `{`で始まる行は同じcollectorが出した接続とupstreamの値なので、protocolとして別に読む。
fn parse_alp_windows(raw: &str, routes: Option<&Path>) -> Result<Vec<Metric>> {
    let mut metrics = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() || line.starts_with('{') {
            continue;
        }
        let (window, json) = line
            .split_once('\t')
            .context("alp window line has no tab after the window")?;
        let parsed = parse_alp_json(json, routes)
            .with_context(|| format!("alp output for window {window} is not valid"))?;
        if window == "whole" {
            metrics.extend(parsed);
            continue;
        }
        let at = window
            .parse::<i64>()
            .ok()
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
            .with_context(|| format!("alp window {window} is not an epoch second"))?;
        metrics.extend(
            parsed
                .into_iter()
                .filter(|metric| match metric.name.as_str() {
                    "http.requests" => !metric.labels.contains_key("status_class"),
                    "http.errors" | "http.request_duration" => true,
                    _ => false,
                })
                .map(|mut metric| {
                    metric.timestamp = Some(at);
                    metric
                }),
        );
    }
    Ok(metrics)
}

fn parse_alp_json(raw: &str, routes: Option<&Path>) -> Result<Vec<Metric>> {
    let normalizer = RouteNormalizer::load(routes)?;
    let value: Value = serde_json::from_str(raw).context("ALP output is not valid JSON")?;
    let records = match value {
        Value::Array(values) if values.first().is_some_and(Value::is_array) => {
            let mut rows = values.into_iter();
            let header = rows
                .next()
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default()
                .into_iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .context("ALP table header must contain strings")
                })
                .collect::<Result<Vec<_>>>()?;
            rows.map(|row| {
                let values = row.as_array().context("ALP table row must be an array")?;
                if values.len() != header.len() {
                    anyhow::bail!(
                        "ALP table row has {} fields but header has {}",
                        values.len(),
                        header.len()
                    );
                }
                Ok(Value::Object(
                    header.iter().cloned().zip(values.iter().cloned()).collect(),
                ))
            })
            .collect::<Result<Vec<_>>>()?
        }
        value => json_records(&serde_json::to_string(&value)?)?,
    };
    #[derive(Default)]
    struct HttpStats {
        count: f64,
        sum_seconds: Option<f64>,
        avg_seconds: Option<f64>,
        min_seconds: Option<f64>,
        max_seconds: Option<f64>,
        percentiles: BTreeMap<&'static str, f64>,
        statuses: BTreeMap<String, f64>,
        response_bytes: Option<f64>,
    }
    let mut routes = BTreeMap::<(String, String), HttpStats>::new();
    for value in &records {
        let Some(object) = value.as_object() else {
            continue;
        };
        let Some(route) = string(object, &["uri", "route", "path"]) else {
            continue;
        };
        let route = normalizer.normalize(route.split('?').next().unwrap_or(&route));
        let method = string(object, &["method"]).unwrap_or_else(|| "-".into());
        let stats = routes.entry((method, route)).or_default();
        stats.count += number(object, &["count", "requests"]).unwrap_or_default();
        if let Some(value) = number(object, &["sum", "sum_time", "request_time_sum"]) {
            *stats.sum_seconds.get_or_insert(0.0) += value;
        }
        stats.avg_seconds =
            number(object, &["avg", "average", "request_time_avg"]).or(stats.avg_seconds);
        if let Some(value) = number(object, &["min", "min_time", "request_time_min"]) {
            stats.min_seconds = Some(
                stats
                    .min_seconds
                    .map_or(value, |current| current.min(value)),
            );
        }
        if let Some(value) = number(object, &["max", "max_time", "request_time_max"]) {
            stats.max_seconds = Some(
                stats
                    .max_seconds
                    .map_or(value, |current| current.max(value)),
            );
        }
        for (quantile, names) in [
            ("0.50", &["p50", "p50_time", "request_time_p50"][..]),
            ("0.95", &["p95", "p95_time", "request_time_p95"][..]),
            ("0.99", &["p99", "p99_time", "request_time_p99"][..]),
        ] {
            if let Some(value) = number(object, names) {
                stats
                    .percentiles
                    .entry(quantile)
                    .and_modify(|current| *current = current.max(value))
                    .or_insert(value);
            }
        }
        for class in ["1xx", "2xx", "3xx", "4xx", "5xx"] {
            if let Some(value) = number(object, &[class]) {
                *stats.statuses.entry(class.into()).or_default() += value;
            }
        }
        if let Some(value) = number(
            object,
            &[
                "response_bytes",
                "body_bytes_sent",
                "sum_body_bytes_sent",
                "sum_body",
            ],
        ) {
            *stats.response_bytes.get_or_insert(0.0) += value;
        }
    }
    let mut metrics = Vec::new();
    for ((method, route), stats) in routes {
        let labels = BTreeMap::from([("method".into(), method), ("route".into(), route)]);
        if stats.count > 0.0 {
            metrics.push(Metric {
                name: "http.requests".into(),
                value: stats.count,
                unit: "requests".into(),
                timestamp: None,
                labels: labels.clone(),
            });
        }
        for (status_class, value) in &stats.statuses {
            let mut status_labels = labels.clone();
            status_labels.insert("status_class".into(), status_class.clone());
            metrics.push(Metric {
                name: "http.requests".into(),
                value: *value,
                unit: "requests".into(),
                timestamp: None,
                labels: status_labels,
            });
        }
        let errors =
            stats.statuses.get("4xx").unwrap_or(&0.0) + stats.statuses.get("5xx").unwrap_or(&0.0);
        if !stats.statuses.is_empty() {
            metrics.push(Metric {
                name: "http.errors".into(),
                value: errors,
                unit: "requests".into(),
                timestamp: None,
                labels: labels.clone(),
            });
        }
        for (name, value) in [
            ("http.request_duration_sum", stats.sum_seconds),
            (
                "http.request_duration_mean",
                stats
                    .sum_seconds
                    .filter(|_| stats.count > 0.0)
                    .map(|sum| sum / stats.count)
                    .or(stats.avg_seconds),
            ),
            ("http.request_duration_min", stats.min_seconds),
            ("http.request_duration_max", stats.max_seconds),
        ] {
            if let Some(seconds) = value {
                metrics.push(Metric {
                    name: name.into(),
                    value: seconds * 1000.0,
                    unit: "ms".into(),
                    timestamp: None,
                    labels: labels.clone(),
                });
            }
        }
        for (quantile, seconds) in stats.percentiles {
            let mut quantile_labels = labels.clone();
            quantile_labels.insert("quantile".into(), quantile.into());
            metrics.push(Metric {
                name: "http.request_duration".into(),
                value: seconds * 1000.0,
                unit: "ms".into(),
                timestamp: None,
                labels: quantile_labels,
            });
        }
        if let Some(bytes) = stats.response_bytes {
            metrics.push(Metric {
                name: "http.response_bytes".into(),
                value: bytes,
                unit: "bytes".into(),
                timestamp: None,
                labels,
            });
        }
    }
    if !records.is_empty() && metrics.is_empty() {
        anyhow::bail!("ALP JSON contained records but no supported count/p95 fields");
    }
    Ok(metrics)
}

fn parse_slp_json(raw: &str) -> Result<Vec<Metric>> {
    let records = json_records(raw)?;
    let mut metrics = Vec::new();
    for value in &records {
        let Some(object) = value.as_object() else {
            continue;
        };
        let Some(digest) = string(object, &["digest", "query", "fingerprint", "abstract"]) else {
            continue;
        };
        let engine = string(object, &["engine"]).unwrap_or_else(|| "mysql".into());
        let labels = BTreeMap::from([("digest".into(), digest), ("engine".into(), engine)]);
        if let Some(calls) = number(object, &["count", "calls", "query_count"]) {
            metrics.push(Metric {
                name: "db.query.calls".into(),
                value: calls,
                unit: "queries".into(),
                timestamp: None,
                labels: labels.clone(),
            });
        }
        if let Some(total_seconds) = number(object, &["total", "total_time", "query_time_sum"]) {
            metrics.push(Metric {
                name: "db.query.total_duration".into(),
                value: total_seconds * 1000.0,
                unit: "ms".into(),
                timestamp: None,
                labels,
            });
        }
    }
    if !records.is_empty() && metrics.is_empty() {
        anyhow::bail!("SLP JSON contained records but no supported calls/duration fields");
    }
    Ok(metrics)
}

fn parse_slp_tsv(raw: &str) -> Result<Vec<Metric>> {
    let mut metrics = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = line.splitn(4, '\t').collect::<Vec<_>>();
        if fields.len() != 4 {
            anyhow::bail!("SLP TSV line {} must contain four fields", index + 1);
        }
        let calls = fields[0]
            .parse::<f64>()
            .with_context(|| format!("invalid SLP count on line {}", index + 1))?;
        let total_seconds = fields[2]
            .parse::<f64>()
            .with_context(|| format!("invalid SLP total query time on line {}", index + 1))?;
        let p95_seconds = fields[3]
            .parse::<f64>()
            .with_context(|| format!("invalid SLP p95 query time on line {}", index + 1))?;
        if !calls.is_finite()
            || calls < 0.0
            || !total_seconds.is_finite()
            || total_seconds < 0.0
            || !p95_seconds.is_finite()
            || p95_seconds < 0.0
        {
            anyhow::bail!("SLP TSV line {} contains an invalid metric", index + 1);
        }
        let labels = BTreeMap::from([
            ("digest".into(), fields[1].to_owned()),
            ("engine".into(), "mysql".into()),
        ]);
        metrics.extend([
            Metric {
                name: "db.query.calls".into(),
                value: calls,
                unit: "queries".into(),
                timestamp: None,
                labels: labels.clone(),
            },
            Metric {
                name: "db.query.total_duration".into(),
                value: total_seconds * 1_000.0,
                unit: "ms".into(),
                timestamp: None,
                labels: labels.clone(),
            },
            Metric {
                name: "db.query.p95_duration".into(),
                value: p95_seconds * 1_000.0,
                unit: "ms".into(),
                timestamp: None,
                labels,
            },
        ]);
    }
    Ok(metrics)
}

/// 表示とindexに載せる文の長さの上限。slpは一括INSERTをまとめるが、念のため切る。
const DIGEST_LIMIT: usize = 1024;

/// `window \t count \t query \t sum \t max \t p95 \t p99 \t lock \t rows_sent \t rows_examined`
/// （時間は秒）。node上で区間（initialize、load、whole）ごとに集計したslpの出力で、
/// `{`で始まる行は同じcollectorが出したDB全体の時系列なので、protocolとして別に読む。
fn parse_slp_windows(raw: &str) -> Vec<Metric> {
    let mut metrics = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() || line.starts_with('{') {
            continue;
        }
        let mut head = line.splitn(3, '\t');
        let (Some(window), Some(count), Some(rest)) = (head.next(), head.next(), head.next())
        else {
            continue;
        };
        // queryにtabが入っても崩れないよう、数値の列は右から取る。
        let mut tail = rest.rsplitn(8, '\t');
        let fields = (0..8).map(|_| tail.next()).collect::<Vec<_>>();
        let [
            Some(rows_examined),
            Some(rows_sent),
            Some(lock),
            Some(p99),
            Some(p95),
            Some(max),
            Some(sum),
            Some(query),
        ] = fields[..]
        else {
            continue;
        };
        let number = |value: &str| value.trim().parse::<f64>().ok();
        let (Some(count), Some(sum)) = (number(count), number(sum)) else {
            continue;
        };
        let mut digest = query.trim().to_owned();
        // 切った文は先頭が同じ別の文と区別できないので、全文のhashを識別に添える。
        let mut digest_id = None;
        if digest.len() > DIGEST_LIMIT {
            use sha2::Digest as _;
            digest_id =
                Some(format!("{:x}", sha2::Sha256::digest(digest.as_bytes()))[..16].to_owned());
            let mut end = DIGEST_LIMIT;
            while !digest.is_char_boundary(end) {
                end -= 1;
            }
            digest.truncate(end);
            digest.push('…');
        }
        let mut labels = BTreeMap::from([
            ("digest".into(), digest),
            ("engine".into(), "mysql".into()),
            ("window".into(), window.to_owned()),
        ]);
        if let Some(digest_id) = digest_id {
            labels.insert("digest_id".into(), digest_id);
        }
        let mut push = |name: &str, value: Option<f64>, unit: &str| {
            if let Some(value) = value {
                metrics.push(Metric {
                    name: name.into(),
                    value,
                    unit: unit.into(),
                    timestamp: None,
                    labels: labels.clone(),
                });
            }
        };
        let ms = |value: Option<f64>| value.map(|seconds| seconds * 1_000.0);
        push("db.query.calls", Some(count), "queries");
        push("db.query.total_duration", Some(sum * 1_000.0), "ms");
        push("db.query.duration_max", ms(number(max)), "ms");
        push("db.query.p95_duration", ms(number(p95)), "ms");
        push("db.query.p99_duration", ms(number(p99)), "ms");
        push("db.query.lock_duration", ms(number(lock)), "ms");
        push("db.query.rows_sent", number(rows_sent), "rows");
        push("db.query.rows_examined", number(rows_examined), "rows");
    }
    metrics
}

fn parse_sysstat(
    raw: &str,
    interval: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
) -> Vec<Metric> {
    let mut cpu_header: Option<Vec<String>> = None;
    let mut disk_header: Option<Vec<String>> = None;
    let mut summaries = BTreeMap::<MetricKey, (f64, u64)>::new();
    let date = regex::Regex::new(r"\b(\d{2}/\d{2}/\d{2})\b")
        .ok()
        .and_then(|pattern| pattern.captures(raw))
        .and_then(|capture| NaiveDate::parse_from_str(&capture[1], "%m/%d/%y").ok());
    let mut clock = SysstatClock { date, last: None };
    let mut metrics = Vec::new();
    for line in raw.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.contains(&"CPU") && fields.contains(&"%idle") {
            cpu_header = Some(fields.iter().map(|field| field.to_string()).collect());
            continue;
        }
        if let Some(header) = &cpu_header {
            let cpu_index = header.iter().position(|field| field == "CPU");
            if fields.len() == header.len() && cpu_index.is_some_and(|index| fields[index] == "all")
            {
                let timestamp = clock.timestamp(&fields);
                if timestamp.is_none() {
                    continue;
                }
                if !within_interval(timestamp, interval) {
                    continue;
                }
                let mut values = BTreeMap::new();
                for (field, name) in [
                    ("%user", "host.cpu_user_percent"),
                    ("%nice", "host.cpu_nice_percent"),
                    ("%system", "host.cpu_system_percent"),
                    ("%iowait", "host.cpu_iowait_percent"),
                    ("%steal", "host.cpu_steal_percent"),
                    ("%idle", "host.cpu_idle_percent"),
                ] {
                    if let Some(value) = sysstat_value(header, &fields, &[field]) {
                        values.insert(name, value);
                        record_metric(
                            &mut metrics,
                            &mut summaries,
                            name,
                            value,
                            "percent",
                            timestamp,
                            BTreeMap::new(),
                        );
                    }
                }
                if let Some(idle) = values.get("host.cpu_idle_percent") {
                    // Busy intentionally excludes iowait. This matches /proc/stat's
                    // host-sampler and keeps CPU saturation separate from storage wait.
                    let busy = (100.0
                        - idle
                        - values
                            .get("host.cpu_iowait_percent")
                            .copied()
                            .unwrap_or_default())
                    .clamp(0.0, 100.0);
                    for name in ["host.cpu_busy_percent", "host.cpu_percent"] {
                        record_metric(
                            &mut metrics,
                            &mut summaries,
                            name,
                            busy,
                            "percent",
                            timestamp,
                            BTreeMap::new(),
                        );
                    }
                }
            }
        }
        if fields.contains(&"DEV") && fields.contains(&"await") {
            disk_header = Some(fields.iter().map(|field| field.to_string()).collect());
            continue;
        }
        let Some(header) = &disk_header else { continue };
        let Some(dev_index) = header.iter().position(|field| field == "DEV") else {
            continue;
        };
        if fields.len() != header.len() {
            continue;
        }
        let device = fields[dev_index].to_string();
        let timestamp = clock.timestamp(&fields);
        if timestamp.is_none() {
            continue;
        }
        if !within_interval(timestamp, interval) {
            continue;
        }
        let labels = BTreeMap::from([("device".into(), device)]);
        for (headers, name, unit, multiplier) in [
            (&["tps"][..], "host.disk_iops", "operations_per_second", 1.0),
            (
                &["rkB/s"][..],
                "host.disk_read_bytes_per_second",
                "bytes_per_second",
                1024.0,
            ),
            (
                &["wkB/s"][..],
                "host.disk_write_bytes_per_second",
                "bytes_per_second",
                1024.0,
            ),
            (
                &["aqu-sz", "avgqu-sz"][..],
                "host.disk_queue_depth",
                "requests",
                1.0,
            ),
            (&["await"][..], "host.disk_await", "ms", 1.0),
            (&["%util"][..], "host.disk_util_percent", "percent", 1.0),
        ] {
            if let Some(value) = sysstat_value(header, &fields, headers) {
                record_metric(
                    &mut metrics,
                    &mut summaries,
                    name,
                    value * multiplier,
                    unit,
                    timestamp,
                    labels.clone(),
                );
            }
        }
    }
    append_metric_summaries(&mut metrics, summaries);
    metrics
}

type MetricKey = (String, String, BTreeMap<String, String>);

fn sysstat_value(header: &[String], fields: &[&str], candidates: &[&str]) -> Option<f64> {
    candidates.iter().find_map(|candidate| {
        header
            .iter()
            .position(|field| field == candidate)
            .and_then(|index| fields[index].replace(',', ".").parse::<f64>().ok())
    })
}

#[allow(clippy::too_many_arguments)]
fn record_metric(
    metrics: &mut Vec<Metric>,
    summaries: &mut BTreeMap<MetricKey, (f64, u64)>,
    name: &str,
    value: f64,
    unit: &str,
    timestamp: Option<chrono::DateTime<Utc>>,
    labels: BTreeMap<String, String>,
) {
    let summary = summaries
        .entry((name.into(), unit.into(), labels.clone()))
        .or_default();
    summary.0 += value;
    summary.1 += 1;
    if let Some(timestamp) = timestamp {
        metrics.push(Metric {
            name: name.into(),
            value,
            unit: unit.into(),
            timestamp: Some(timestamp),
            labels,
        });
    }
}

fn append_metric_summaries(metrics: &mut Vec<Metric>, summaries: BTreeMap<MetricKey, (f64, u64)>) {
    metrics.extend(
        summaries
            .into_iter()
            .filter_map(|((name, unit, labels), (sum, count))| {
                (count > 0).then_some(Metric {
                    name,
                    value: sum / count as f64,
                    unit,
                    timestamp: None,
                    labels,
                })
            }),
    );
}

fn parse_service_cgroup(
    raw: &str,
    interval: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
) -> Vec<Metric> {
    type Counters = (chrono::DateTime<Utc>, u64, u64, u64);
    let mut previous = BTreeMap::<String, Counters>::new();
    let mut summaries = BTreeMap::<MetricKey, (f64, u64)>::new();
    let mut metrics = Vec::new();
    for line in raw.lines() {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 7 {
            continue;
        }
        let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(fields[0]) else {
            continue;
        };
        let timestamp = timestamp.with_timezone(&Utc);
        let service = fields[1].to_string();
        let Ok(cpu_usec) = fields[2].parse::<u64>() else {
            continue;
        };
        let Ok(memory) = fields[3].parse::<u64>() else {
            continue;
        };
        let Ok(read_bytes) = fields[4].parse::<u64>() else {
            continue;
        };
        let Ok(write_bytes) = fields[5].parse::<u64>() else {
            continue;
        };
        let Ok(pids) = fields[6].parse::<u64>() else {
            continue;
        };
        let prior = previous.insert(
            service.clone(),
            (timestamp, cpu_usec, read_bytes, write_bytes),
        );
        if !within_interval(Some(timestamp), interval) {
            continue;
        }
        let labels = BTreeMap::from([("service".into(), service)]);
        record_metric(
            &mut metrics,
            &mut summaries,
            "service.memory_bytes",
            memory as f64,
            "bytes",
            Some(timestamp),
            labels.clone(),
        );
        record_metric(
            &mut metrics,
            &mut summaries,
            "service.pids",
            pids as f64,
            "processes",
            Some(timestamp),
            labels.clone(),
        );
        let Some((prior_at, prior_cpu, prior_read, prior_write)) = prior else {
            continue;
        };
        let elapsed = (timestamp - prior_at)
            .num_microseconds()
            .unwrap_or_default();
        if elapsed <= 0
            || cpu_usec < prior_cpu
            || read_bytes < prior_read
            || write_bytes < prior_write
        {
            continue;
        }
        for (name, value, unit) in [
            (
                "service.cpu_cores",
                (cpu_usec - prior_cpu) as f64 / elapsed as f64,
                "cores",
            ),
            (
                "service.io_read_bytes_per_second",
                (read_bytes - prior_read) as f64 * 1_000_000.0 / elapsed as f64,
                "bytes_per_second",
            ),
            (
                "service.io_write_bytes_per_second",
                (write_bytes - prior_write) as f64 * 1_000_000.0 / elapsed as f64,
                "bytes_per_second",
            ),
        ] {
            record_metric(
                &mut metrics,
                &mut summaries,
                name,
                value,
                unit,
                Some(timestamp),
                labels.clone(),
            );
        }
    }
    append_metric_summaries(&mut metrics, summaries);
    metrics
}

fn within_interval(
    timestamp: Option<chrono::DateTime<Utc>>,
    interval: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
) -> bool {
    interval.is_none_or(|(start, end)| timestamp.is_some_and(|at| at >= start && at <= end))
}

/// sarは日付を出力の先頭に1回しか書かず、各行は時刻だけを持つ。0時（TZ=UTCなので
/// JSTの9時）をまたぐと時刻が戻るので、そこで日付を1日進める。
struct SysstatClock {
    date: Option<NaiveDate>,
    last: Option<NaiveDateTime>,
}

impl SysstatClock {
    fn timestamp(&mut self, fields: &[&str]) -> Option<chrono::DateTime<Utc>> {
        let value = match fields.get(1).copied() {
            Some("AM" | "PM") => format!("{} {}", fields.first()?, fields[1]),
            _ => fields.first()?.to_string(),
        };
        let time = NaiveTime::parse_from_str(&value, "%H:%M:%S")
            .or_else(|_| NaiveTime::parse_from_str(&value, "%I:%M:%S %p"))
            .ok()?;
        let mut at = NaiveDateTime::new(self.date?, time);
        if let Some(last) = self.last
            && at + chrono::Duration::hours(12) < last
        {
            let next = self.date?.succ_opt()?;
            self.date = Some(next);
            at = NaiveDateTime::new(next, time);
        }
        self.last = Some(at);
        Some(Utc.from_utc_datetime(&at))
    }
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
                // The benchmark's own machine is part of the rules; never measure it.
                !node.rule_side
                    && (collector.roles.is_empty()
                        || collector.roles.iter().any(|role| node.roles.contains(role)))
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
    let route_matching_groups = if collector
        .command
        .iter()
        .any(|argument| argument.contains("{route_matching_groups}"))
    {
        let path = config.project_root.join(".isuscope/routes.toml");
        Some(
            RouteNormalizer::load(path.is_file().then_some(path.as_path()))?
                .alp_matching_groups()?,
        )
    } else {
        None
    };
    let windows = RunWindows::read(run_dir);
    let expanded = collector
        .command
        .iter()
        .map(|argument| {
            replace_placeholders(
                argument,
                run_id,
                run_dir,
                node,
                route_matching_groups.as_deref(),
                &config.config.observability.service_units,
            )
            .replace("{benchmark_started_at}", &windows.started_at)
            .replace("{load_started_at}", &windows.load_started_at)
            .replace("{benchmark_finished_at}", &windows.finished_at)
        })
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
        let mut ssh_args = config.ssh_options();
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
        program: self_program(program),
        args: args.to_vec(),
        id_prefix,
        working_dir: config.project_root.clone(),
    })
}

/// `isuscope`を呼ぶlocal collector（`__transition`など）は、PATH上のものではなく今動いている
/// binaryで実行します。PATHに古い版が残っていると、同じrunの中でmetricの名前や計算が
/// 食い違うためです。
fn self_program(program: &str) -> String {
    if program == "isuscope"
        && let Ok(current) = std::env::current_exe()
    {
        return current.display().to_string();
    }
    program.to_owned()
}

/// after phaseのcollectorへ渡す区間の境界（epoch秒）。分からなければ空文字列で、
/// collectorは区間を分けずに全体（whole）として集計する。
struct RunWindows {
    started_at: String,
    load_started_at: String,
    finished_at: String,
}

impl RunWindows {
    fn read(run_dir: &Path) -> Self {
        let manifest = fs::read(run_dir.join("run.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<crate::model::RunManifest>(&raw).ok());
        let epoch = |value: Option<chrono::DateTime<Utc>>| {
            value
                .map(|at| format!("{:.3}", at.timestamp_micros() as f64 / 1_000_000.0))
                .unwrap_or_default()
        };
        Self {
            started_at: epoch(
                manifest
                    .as_ref()
                    .and_then(|manifest| manifest.benchmark.started_at),
            ),
            load_started_at: epoch(
                manifest
                    .as_ref()
                    .and_then(|manifest| manifest.benchmark.initialize_finished_at),
            ),
            finished_at: epoch(
                manifest
                    .as_ref()
                    .and_then(|manifest| manifest.benchmark.finished_at),
            ),
        }
    }
}

fn replace_placeholders(
    value: &str,
    run_id: &str,
    run_dir: &Path,
    node: Option<&NodeConfig>,
    route_matching_groups: Option<&str>,
    service_units: &[String],
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
        .replace(
            "{route_matching_groups}",
            route_matching_groups.unwrap_or_default(),
        )
        .replace("{service_units}", &service_units.join(" "))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn sanitize(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alp_windows_keep_the_whole_delta_and_five_second_buckets_apart() {
        // 実際のcollectorとalp 1.0.21で、`access-ltsv-windows.log`を集計した出力。
        let raw = include_str!("../tests/fixtures/alp-windows-v1.0.21.out");
        let dir = tempfile::tempdir().unwrap();
        let routes = dir.path().join("routes.toml");
        fs::write(
            &routes,
            "[[routes]]\npattern = '^/user/[0-9]+/home$'\nreplace = '/user/:id/home'\n",
        )
        .unwrap();
        let metrics = parse_alp_windows(raw, Some(&routes)).unwrap();
        let value = |name: &str, route: &str, at: Option<i64>, extra: Option<(&str, &str)>| {
            metrics
                .iter()
                .find(|metric| {
                    metric.name == name
                        && metric.labels["route"] == route
                        && metric.timestamp.map(|at| at.timestamp()) == at
                        && extra.is_none_or(|(key, value)| {
                            metric.labels.get(key).map(String::as_str) == Some(value)
                        })
                        && (extra.is_some() || !metric.labels.contains_key("quantile"))
                })
                .map(|metric| metric.value)
        };
        // 差分全体：ベンチ区間の外の要求も数え、時間は`$request_time`（`apptime`ではない）。
        assert_eq!(value("http.requests", "/initialize", None, None), Some(1.0));
        assert_eq!(
            value("http.requests", "/user/:id/home", None, None),
            Some(4.0)
        );
        assert_eq!(
            value("http.request_duration_sum", "/user/:id/home", None, None),
            Some(100.0)
        );
        assert_eq!(
            value("http.response_bytes", "/user/:id/home", None, None),
            Some(400.0)
        );
        assert_eq!(value("http.errors", "/login", None, None), Some(1.0));
        // 5秒bucket：区間の中だけで、回数・error・分位点だけを時系列にする。
        let first = Some(1_789_653_660);
        assert_eq!(
            value("http.requests", "/user/:id/home", first, None),
            Some(3.0)
        );
        assert_eq!(value("http.errors", "/login", first, None), Some(1.0));
        assert_eq!(
            value(
                "http.request_duration",
                "/user/:id/home",
                Some(1_789_653_665),
                Some(("quantile", "0.95"))
            ),
            Some(40.0)
        );
        assert_eq!(value("http.requests", "/initialize", first, None), None);
        assert!(!metrics.iter().any(|metric| metric.timestamp.is_some()
            && (metric.labels.contains_key("status_class")
                || metric.name == "http.request_duration_sum")));
        // 接続とupstreamの値（`{`の行）はprotocolとして別に読むので、ここには入らない。
        assert!(
            !metrics
                .iter()
                .any(|metric| metric.name.starts_with("client."))
        );
    }

    #[test]
    fn alp_collector_reports_connections_and_upstreams_as_protocol_metrics() {
        let raw = include_str!("../tests/fixtures/alp-windows-v1.0.21.out");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alp.zst");
        fs::write(&path, zstd::encode_all(raw.as_bytes(), 1).unwrap()).unwrap();
        let (metrics, _, _) = parse_protocol(&path).unwrap();
        let find = |name: &str, labels: &[(&str, &str)], at: Option<i64>| {
            metrics
                .iter()
                .find(|metric| {
                    metric.name == name
                        && metric.timestamp.map(|at| at.timestamp()) == at
                        && labels.iter().all(|(key, value)| {
                            metric.labels.get(*key).map(String::as_str) == Some(value)
                        })
                })
                .map(|metric| metric.value)
        };
        // 区間の中の接続は3本（2番は3要求）。区間の外の`/initialize`と`/health`は数えない。
        assert_eq!(
            find("client.connections_opened_total", &[], None),
            Some(3.0)
        );
        assert_eq!(find("client.connection_requests_max", &[], None), Some(3.0));
        assert_eq!(
            find("client.connections_opened", &[], Some(1_789_653_660)),
            Some(2.0)
        );
        // 10.0.0.9で失敗し10.0.0.1で返った要求は、時間を試行ごとの接続先へ付ける（`-`は数えない）。
        // 合計を最後の接続先へ付けると、正常な10.0.0.1が遅く見え、失敗した10.0.0.9が消える。
        let failed = [("upstream", "10.0.0.9:8080")];
        assert_eq!(find("http.upstream_requests", &failed, None), Some(1.0));
        assert_eq!(
            find("http.upstream_retried_requests", &failed, None),
            Some(1.0)
        );
        assert_eq!(
            find("http.upstream_response_duration_max", &failed, None),
            Some(2.0)
        );
        let upstream = [("upstream", "10.0.0.1:8080")];
        assert_eq!(find("http.upstream_requests", &upstream, None), Some(3.0));
        assert_eq!(
            find("http.upstream_retried_requests", &upstream, None),
            None
        );
        assert_eq!(
            find("http.upstream_connect_duration_max", &upstream, None),
            Some(1.0)
        );
        assert_eq!(
            find(
                "http.upstream_header_duration",
                &[("upstream", "10.0.0.1:8080"), ("quantile", "0.50")],
                None
            ),
            Some(28.0)
        );
        assert!(!metrics.iter().any(|metric| {
            metric
                .labels
                .get("upstream")
                .is_some_and(|upstream| upstream == "-")
        }));
    }

    #[test]
    fn slp_windows_keep_initialize_and_load_apart() {
        // 実際のcollectorとslp 0.2.1で、practice-12のslow logの抜粋を集計した出力。
        let raw = include_str!("../tests/fixtures/slp-windows-v0.2.1.out");
        let metrics = parse_slp_windows(raw);
        let value = |window: &str, name: &str, digest: &str| {
            metrics
                .iter()
                .find(|metric| {
                    metric.name == name
                        && metric.labels["window"] == window
                        && metric.labels["digest"].contains(digest)
                })
                .map(|metric| metric.value)
        };
        // 負荷区間のid_generatorは7回（ベンチ終了後に流れた1回は区間外として捨てた）。
        assert_eq!(value("load", "db.query.calls", "id_generator"), Some(7.0));
        assert_eq!(
            value("load", "db.query.total_duration", "id_generator").map(|ms| ms.round()),
            Some(221.0)
        );
        assert!(value("load", "db.query.p99_duration", "id_generator").is_some());
        assert!(value("load", "db.query.duration_max", "id_generator").is_some());
        assert_eq!(
            value("load", "db.query.rows_examined", "COUNT(N)"),
            Some(623.0)
        );
        // 一括INSERTはVALUESが1つにまとまり、initializeの区間に入る。
        assert_eq!(
            value("initialize", "db.query.calls", "INSERT INTO `user_cards`"),
            Some(1.0)
        );
        assert_eq!(
            value("load", "db.query.calls", "INSERT INTO `user_cards`"),
            None
        );
        // PingやPrepareはslpもawkも文として数えない。
        assert!(
            !metrics
                .iter()
                .any(|metric| metric.labels["digest"].contains("administrator"))
        );
        // DB全体の時系列（`{`の行）はprotocolとして別に読むので、ここには入らない。
        assert!(!metrics.iter().any(|metric| metric.name == "db.calls"));
    }

    #[test]
    fn slp_windows_cap_long_statements_and_survive_tabs_in_queries() {
        let long = format!("SELECT '{}'", "x".repeat(4000));
        let raw = format!(
            "load\t3\t{long}\t0.3\t0.2\t0.2\t0.2\t0\t1\t1\nload\t1\tSELECT a\tb\t0.1\t0.1\t0.1\t0.1\t0\t2\t3\n"
        );
        let metrics = parse_slp_windows(&raw);
        let digests = metrics
            .iter()
            .filter(|metric| metric.name == "db.query.calls")
            .map(|metric| metric.labels["digest"].clone())
            .collect::<Vec<_>>();
        assert!(
            digests[0].chars().count() <= DIGEST_LIMIT + 1,
            "{}",
            digests[0].len()
        );
        assert!(digests[0].ends_with('…'));
        assert_eq!(digests[1], "SELECT a\tb");
    }

    #[test]
    fn slp_windows_keep_long_statements_with_the_same_prefix_apart() {
        // 列の並びが上限より長く、表だけが違う2文は、切ると同じ文字列になる。
        let columns = (0..200)
            .map(|index| format!("col_{index:03}"))
            .collect::<Vec<_>>()
            .join(", ");
        let raw = format!(
            "load\t10\tSELECT {columns} FROM a\t1.0\t0.1\t0.1\t0.1\t0\t10\t10\n\
             load\t20\tSELECT {columns} FROM b\t8.0\t0.4\t0.4\t0.4\t0\t20\t20\n"
        );
        let rows = crate::report::database_queries(&parse_slp_windows(&raw));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].digest, rows[1].digest);
        assert_ne!(rows[0].digest_id, rows[1].digest_id);
        assert_eq!(rows.iter().map(|row| row.calls).sum::<f64>(), 30.0);
        assert_eq!(rows.iter().map(|row| row.total_ms).sum::<f64>(), 9_000.0);
        // 上限に収まる文には付けない（過去のrunとの照合を変えない）。
        let short = parse_slp_windows("load\t1\tSELECT 1\t0.1\t0.1\t0.1\t0.1\t0\t1\t1\n");
        assert!(!short[0].labels.contains_key("digest_id"));
    }

    #[test]
    fn perf_samples_on_the_monotonic_clock_become_wall_time() {
        // perf-startが残す壁時計とuptime。sampleの時刻はCLOCK_MONOTONIC（uptimeと同じ基準）。
        let raw = "# isuscope-perf-clock 1787827200.250000000 1000.25\n\
nginx 1200 [000] 1000.350000000: cycles: 7f00 ngx_http_handler (/usr/local/sbin/nginx)\n\
nginx 1200 [001] 1006.000000000: cycles: 7f01 ngx_http_handler (/usr/local/sbin/nginx)\n";
        let metrics = parse_perf_script(raw).unwrap();
        let buckets = metrics
            .iter()
            .filter(|metric| metric.name == "cpu.sample_count")
            .filter_map(|metric| metric.timestamp.map(|at| at.to_rfc3339()))
            .collect::<std::collections::BTreeSet<_>>();
        // 1787827200.35 → 5秒bucketの1787827200、1787827206.0 → 1787827205。
        assert_eq!(
            buckets.into_iter().collect::<Vec<_>>(),
            ["2026-08-27T10:40:00+00:00", "2026-08-27T10:40:05+00:00"]
        );

        // busyboxのdateは`%N`を出さないので、小数部の無い壁時計も読める。
        assert!(parse_perf_script("# isuscope-perf-clock 1787827200. 1000.25\n").is_ok());

        // 時計の基準が合わない（別の時計で記録された）なら、黙って別の時刻へ置かずに失敗する。
        let mismatched = "# isuscope-perf-clock 1787827200.25 1000.25\n\
nginx 1200 [000] 900000.0: cycles: 7f00 ngx_http_handler (/usr/local/sbin/nginx)\n";
        assert!(parse_perf_script(mismatched).is_err());
    }

    #[test]
    fn sysstat_samples_after_midnight_move_to_the_next_day() {
        let raw = "Linux 5.15.0 (app1) \t09/18/26 \t_x86_64_\t(2 CPU)\n\n\
23:59:59        CPU     %user     %nice   %system   %iowait    %steal     %idle\n\
23:59:59        all     10.00      0.00      5.00      0.00      0.00     85.00\n\
00:00:00        CPU     %user     %nice   %system   %iowait    %steal     %idle\n\
00:00:00        all     20.00      0.00      5.00      0.00      0.00     75.00\n";
        let interval = Some((
            "2026-09-18T23:59:58Z".parse().unwrap(),
            "2026-09-19T00:00:02Z".parse().unwrap(),
        ));
        let metrics = parse_sysstat(raw, interval);
        let busy = metrics
            .iter()
            .filter(|metric| metric.name == "host.cpu_busy_percent" && metric.timestamp.is_some())
            .map(|metric| (metric.timestamp.unwrap().to_rfc3339(), metric.value))
            .collect::<Vec<_>>();
        assert_eq!(
            busy,
            [
                ("2026-09-18T23:59:59+00:00".to_owned(), 15.0),
                ("2026-09-19T00:00:00+00:00".to_owned(), 25.0),
            ]
        );
    }

    #[test]
    fn sql_digests_keep_digits_inside_identifiers() {
        assert_eq!(
            normalize_sql_digest("SELECT * FROM user_items_2 t1 WHERE t1.id = 42 LIMIT 10"),
            "select * from user_items_2 t1 where t1.id = ? limit ?"
        );
        assert_eq!(
            normalize_sql_digest("UPDATE t SET c=0x1F, d=-3.5e2 WHERE `col9` IN (1,2)"),
            "update t set c=?,d=-? where `col9` in (?,?)"
        );
    }

    #[test]
    fn profile_artifacts_require_svg_and_folded_shapes() {
        let directory = tempfile::tempdir().unwrap();
        let write = |name: &str, value: &str| {
            let path = directory.path().join(name);
            let output = fs::File::create(&path).unwrap();
            let mut encoder = zstd::stream::write::Encoder::new(output, 1).unwrap();
            use std::io::Write;
            encoder.write_all(value.as_bytes()).unwrap();
            encoder.finish().unwrap();
            path
        };
        assert!(
            validate_profile_artifact(
                "perf-flamegraph",
                &write(
                    "valid-svg.zst",
                    include_str!("../tests/fixtures/perf-flamegraph-isucon13-normalized.svg")
                )
            )
            .is_ok()
        );
        assert!(
            validate_profile_artifact("perf-flamegraph", &write("invalid-svg.zst", "<svg>"))
                .is_err()
        );
        assert!(
            validate_profile_artifact(
                "offcpu",
                &write(
                    "valid-folded.zst",
                    include_str!("../tests/fixtures/offcpu-isucon13.folded")
                )
            )
            .is_ok()
        );
        assert!(validate_profile_artifact("offcpu", &write("empty-folded.zst", "")).is_err());
        assert!(
            validate_profile_artifact("offcpu", &write("invalid-folded.zst", "not-folded"))
                .is_err()
        );

        let acceptance: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/profile-acceptance-isucon13.json"
        ))
        .unwrap();
        assert_eq!(acceptance["normal"]["nodes"], 3);
        assert_eq!(acceptance["empty_sample"]["offcpu"], "unavailable");
        assert_eq!(
            acceptance["missing_dependency"]["stackcollapse_perf"],
            "unavailable"
        );
    }

    #[test]
    fn standard_json_adapters_emit_common_metrics() {
        let alp = parse_alp_json(
            include_str!("../tests/fixtures/alp-json-current.json"),
            None,
        )
        .unwrap();
        assert!(
            alp.iter()
                .any(|metric| metric.name == "http.requests" && metric.value == 12.0)
        );
        assert!(
            alp.iter()
                .any(|metric| metric.name == "http.request_duration" && metric.value == 125.0)
        );
        assert!(
            alp.iter().any(|metric| {
                metric.name == "http.request_duration_sum" && metric.value == 960.0
            })
        );
        assert!(alp.iter().any(|metric| {
            metric.name == "http.request_duration"
                && metric.value == 400.0
                && metric.labels.get("quantile").map(String::as_str) == Some("0.99")
        }));
        assert!(
            alp.iter()
                .any(|metric| metric.name == "http.errors" && metric.value == 2.0)
        );
        assert!(
            alp.iter()
                .any(|metric| { metric.name == "http.response_bytes" && metric.value == 24_000.0 })
        );

        let slp = parse_slp_json(include_str!("../tests/fixtures/slp-json-current.json")).unwrap();
        assert!(
            slp.iter()
                .any(|metric| metric.name == "db.query.calls" && metric.value == 4.0)
        );
        assert!(
            slp.iter()
                .any(|metric| metric.name == "db.query.total_duration" && metric.value == 800.0)
        );
        assert!(parse_alp_json(r#"[{"url":"/unknown","number":1}]"#, None).is_err());
        assert!(parse_slp_json(r#"[{"sql":"SELECT 1","elapsed":1}]"#).is_err());
    }

    #[test]
    fn current_alp_table_json_and_slp_tsv_emit_common_metrics() {
        let alp = parse_alp_json(
            include_str!("../tests/fixtures/alp-json-v1.0.21.json"),
            None,
        )
        .unwrap();
        assert!(alp.iter().any(|metric| {
            metric.name == "http.requests"
                && metric.value == 2.0
                && metric.labels.get("route").map(String::as_str) == Some("/api/items/1")
        }));
        assert!(
            alp.iter()
                .any(|metric| { metric.name == "http.request_duration" && metric.value == 20.0 })
        );
        assert!(
            parse_alp_json(
                include_str!("../tests/fixtures/alp-json-v1.0.21-empty.json"),
                None,
            )
            .unwrap()
            .is_empty()
        );

        let statuses = parse_alp_json(
            include_str!("../tests/fixtures/alp-json-v1.0.21-statuses.json"),
            None,
        )
        .unwrap();
        assert!(statuses.iter().any(|metric| {
            metric.name == "http.errors"
                && metric.value == 2.0
                && metric.labels.get("route").map(String::as_str) == Some("/api/items/1")
        }));
        for (status, count) in [("2xx", 1.0), ("3xx", 1.0), ("4xx", 1.0), ("5xx", 1.0)] {
            assert!(statuses.iter().any(|metric| {
                metric.name == "http.requests"
                    && metric.value == count
                    && metric.labels.get("status_class").map(String::as_str) == Some(status)
            }));
        }

        let missing = parse_alp_json(
            include_str!("../tests/fixtures/alp-json-v1.0.21-missing-fields.json"),
            None,
        )
        .unwrap();
        assert!(missing.iter().any(|metric| {
            metric.name == "http.requests"
                && metric.value == 1.0
                && metric.labels.get("method").map(String::as_str) == Some("")
        }));
        assert!(missing.iter().any(|metric| {
            metric.name == "http.requests"
                && metric.value == 2.0
                && metric.labels.get("method").map(String::as_str) == Some("GET")
        }));
        assert!(parse_alp_json(r#"[["count","uri"],[1]]"#, None).is_err());

        let slp = parse_slp_tsv(include_str!("../tests/fixtures/slp-tsv-v0.2.1.tsv")).unwrap();
        assert!(slp.iter().any(|metric| {
            metric.name == "db.query.calls"
                && metric.value == 3.0
                && metric.labels.get("digest").map(String::as_str)
                    == Some("SELECT * FROM users WHERE id = ?")
        }));
        assert!(
            slp.iter().any(|metric| {
                metric.name == "db.query.total_duration" && metric.value == 800.0
            })
        );
        assert!(
            slp.iter()
                .any(|metric| { metric.name == "db.query.p95_duration" && metric.value == 400.0 })
        );
        assert!(parse_slp_tsv("").unwrap().is_empty());
        assert!(parse_slp_tsv("not tsv").is_err());
    }

    #[test]
    fn perf_script_is_bucketed_by_symbol_and_process() {
        let metrics =
            parse_perf_script(include_str!("../tests/fixtures/perf-script-series.txt")).unwrap();
        assert!(metrics.iter().any(|metric| {
            metric.name == "cpu.sample_count"
                && metric.value == 2.0
                && metric.labels.get("process").map(String::as_str) == Some("nginx")
                && metric.timestamp.is_some()
        }));
        assert!(metrics.iter().any(|metric| {
            metric.name == "cpu.sample_count"
                && metric.value == 1.0
                && metric.labels.get("process").map(String::as_str) == Some("isupipe-rust")
        }));
    }

    #[test]
    fn perf_script_also_reports_the_whole_capture_by_symbol() {
        // perf reportを別に走らせず、同じperf scriptからrun全体の自己時間の割合を出す。
        let metrics =
            parse_perf_script(include_str!("../tests/fixtures/perf-script-series.txt")).unwrap();
        let whole = metrics
            .iter()
            .filter(|metric| metric.name == "cpu.sample_percent" && metric.timestamp.is_none())
            .collect::<Vec<_>>();
        assert!((whole.iter().map(|metric| metric.value).sum::<f64>() - 100.0).abs() < 1e-9);
        let nginx = whole
            .iter()
            .find(|metric| metric.labels["symbol"] == "ngx_http_handler")
            .unwrap();
        assert!((nginx.value - 200.0 / 3.0).abs() < 1e-9);
        assert_eq!(nginx.labels["binary"], "nginx");
    }

    #[test]
    fn perf_script_with_hidden_call_graph_reads_right_aligned_headers() {
        let metrics = parse_perf_script(include_str!(
            "../tests/fixtures/perf-script-hidden-callchain.txt"
        ))
        .unwrap();
        let counts = metrics
            .iter()
            .filter(|metric| metric.name == "cpu.sample_count" && metric.timestamp.is_some())
            .collect::<Vec<_>>();
        assert_eq!(counts.iter().map(|metric| metric.value).sum::<f64>(), 3.0);
        assert!(counts.iter().any(|metric| {
            metric.labels.get("process").map(String::as_str) == Some("98-reboot-requi")
                && metric.labels.get("binary").map(String::as_str) == Some("ld-linux-x86-64.so.2")
        }));
        assert!(counts.iter().any(|metric| {
            metric.labels.get("process").map(String::as_str) == Some("actix-rt|system")
                && metric
                    .labels
                    .get("symbol")
                    .map(String::as_str)
                    .is_some_and(|symbol| symbol.ends_with("::encrypt"))
        }));
    }

    #[test]
    fn perf_script_with_call_graph_uses_the_leaf_frame() {
        let metrics =
            parse_perf_script(include_str!("../tests/fixtures/perf-script-callchain.txt")).unwrap();
        let counts = metrics
            .iter()
            .filter(|metric| metric.name == "cpu.sample_count" && metric.timestamp.is_some())
            .collect::<Vec<_>>();
        assert_eq!(counts.iter().map(|metric| metric.value).sum::<f64>(), 4.0);
        let label =
            |metric: &&Metric, key: &str| metric.labels.get(key).cloned().unwrap_or_default();
        assert!(
            counts
                .iter()
                .any(|metric| label(metric, "process") == "swapper"
                    && label(metric, "symbol") == "native_safe_halt"
                    && label(metric, "binary") == "[kernel.kallsyms]")
        );
        assert!(
            counts
                .iter()
                .any(|metric| label(metric, "process") == "actix-rt|system"
                    && label(metric, "binary") == "isuconquest"
                    && label(metric, "symbol").ends_with("::perhaps_write_key_update"))
        );
        assert!(
            counts
                .iter()
                .any(|metric| label(metric, "process") == "actix-rt|system"
                    && label(metric, "symbol") == "__send")
        );
        // A header without frames still counts as CPU time of that process.
        assert!(
            counts
                .iter()
                .any(|metric| label(metric, "process") == "nginx"
                    && label(metric, "symbol") == "[unknown]")
        );
        assert!(
            counts
                .iter()
                .all(|metric| !label(metric, "process").contains('<'))
        );
        let whole_run = |process: &str| {
            metrics
                .iter()
                .find(|metric| {
                    metric.name == "cpu.process_percent"
                        && metric.timestamp.is_none()
                        && metric.labels.get("process").map(String::as_str) == Some(process)
                })
                .map(|metric| metric.value)
        };
        assert_eq!(whole_run("actix-rt|system"), Some(50.0));
        assert_eq!(whole_run("swapper"), Some(25.0));
    }

    #[test]
    fn alp_adapter_uses_shared_route_rules() {
        let directory = tempfile::tempdir().unwrap();
        let rules = directory.path().join("routes.toml");
        fs::write(
            &rules,
            "[[routes]]\npattern = \"^/items/[0-9]+$\"\nreplace = \"/items/:id\"\n",
        )
        .unwrap();
        let metrics = parse_alp_json(
            r#"[{"count":12,"method":"GET","uri":"/items/42?x=1","p95":0.125},{"count":8,"method":"GET","uri":"/items/43","p95":0.150}]"#,
            Some(&rules),
        )
        .unwrap();
        assert!(metrics.iter().all(|metric| {
            metric.labels.get("route").map(String::as_str) == Some("/items/:id")
        }));
        assert!(
            metrics
                .iter()
                .any(|metric| metric.name == "http.requests" && metric.value == 20.0)
        );
        assert!(
            metrics
                .iter()
                .any(|metric| metric.name == "http.request_duration" && metric.value == 150.0)
        );
    }

    #[test]
    fn sysstat_adapter_uses_during_samples() {
        let metrics = parse_sysstat(
            include_str!("../tests/fixtures/sysstat-sysstat12-12h.txt"),
            None,
        );
        assert!(
            metrics
                .iter()
                .any(|metric| metric.name == "host.cpu_percent" && metric.value == 4.5)
        );
        assert!(
            metrics
                .iter()
                .any(|metric| metric.name == "host.disk_await" && metric.value == 5.0)
        );
        assert!(
            metrics
                .iter()
                .any(|metric| metric.name == "host.disk_util_percent" && metric.value == 80.0)
        );
        assert!(metrics.iter().any(|metric| {
            metric.name == "host.cpu_busy_percent"
                && metric.timestamp.is_none()
                && metric.value == 4.5
        }));
        assert!(metrics.iter().any(|metric| {
            metric.name == "host.disk_iops" && metric.timestamp.is_none() && metric.value == 10.0
        }));
        assert!(metrics.iter().any(|metric| {
            metric.name == "host.disk_write_bytes_per_second"
                && metric.timestamp.is_none()
                && metric.value == 2048.0
        }));
        assert!(metrics.iter().any(|metric| {
            metric.name == "host.disk_queue_depth"
                && metric.timestamp.is_none()
                && metric.value == 0.1
        }));
    }

    #[test]
    fn sysstat_adapter_preserves_sample_timestamps() {
        let metrics = parse_sysstat(
            include_str!("../tests/fixtures/sysstat-sysstat12-24h.txt"),
            Some((
                "2026-08-27T12:00:01Z".parse().unwrap(),
                "2026-08-27T12:00:01Z".parse().unwrap(),
            )),
        );
        assert!(metrics.iter().any(|metric| {
            metric.name == "host.cpu_percent"
                && metric.timestamp.map(|at| at.to_rfc3339())
                    == Some("2026-08-27T12:00:01+00:00".into())
        }));
        assert!(
            metrics
                .iter()
                .any(|metric| { metric.name == "host.disk_await" && metric.timestamp.is_some() })
        );
        assert!(
            metrics
                .iter()
                .filter_map(|metric| metric.timestamp)
                .all(|at| {
                    at == "2026-08-27T12:00:01Z"
                        .parse::<chrono::DateTime<Utc>>()
                        .unwrap()
                })
        );
    }

    #[test]
    fn sysstat_adapter_accepts_supported_ubuntu_package_versions() {
        let fixtures = [
            include_str!("../tests/fixtures/sysstat-ubuntu-20.04-sysstat-12.2.0.txt"),
            include_str!("../tests/fixtures/sysstat-ubuntu-22.04-sysstat-12.5.2.txt"),
            include_str!("../tests/fixtures/sysstat-ubuntu-24.04-sysstat-12.6.1.txt"),
        ];
        for fixture in fixtures {
            let metrics = parse_sysstat(fixture, None);
            assert!(
                metrics
                    .iter()
                    .any(|metric| { metric.name == "host.cpu_percent" && metric.value > 0.0 })
            );
            assert!(metrics.iter().any(|metric| {
                metric.name == "host.disk_await" && metric.labels.contains_key("device")
            }));
            assert!(metrics.iter().any(|metric| {
                metric.name == "host.disk_util_percent" && metric.labels.contains_key("device")
            }));
            assert!(metrics.iter().all(|metric| metric.value.is_finite()));
        }
    }

    #[test]
    fn sysstat_busy_cpu_excludes_iowait() {
        let metrics = parse_sysstat(
            include_str!("../tests/fixtures/sysstat-ubuntu-22.04-sysstat-12.5.2.txt"),
            None,
        );
        assert!(metrics.iter().any(|metric| {
            metric.name == "host.cpu_iowait_percent"
                && metric.timestamp.is_some()
                && (metric.value - 18.66).abs() < 0.000_001
        }));
        assert!(metrics.iter().any(|metric| {
            metric.name == "host.cpu_busy_percent"
                && metric.timestamp.is_some()
                && (metric.value - 2.73).abs() < 0.000_001
        }));
    }

    #[test]
    fn service_cgroup_adapter_calculates_rates_and_keeps_gauges() {
        let metrics = parse_service_cgroup(
            "2026-08-27T12:00:00.000000000Z\tisu.service\t1000000\t1048576\t100\t200\t3\n\
             2026-08-27T12:00:01.000000000Z\tisu.service\t2500000\t2097152\t1124\t2248\t4\n",
            None,
        );
        assert!(metrics.iter().any(|metric| {
            metric.name == "service.cpu_cores"
                && metric.timestamp.is_some()
                && metric.value == 1.5
                && metric.labels.get("service").map(String::as_str) == Some("isu.service")
        }));
        assert!(metrics.iter().any(|metric| {
            metric.name == "service.io_read_bytes_per_second"
                && metric.timestamp.is_some()
                && metric.value == 1024.0
        }));
        assert!(metrics.iter().any(|metric| {
            metric.name == "service.memory_bytes"
                && metric.timestamp.is_none()
                && metric.value == 1_572_864.0
        }));
        assert!(metrics.iter().any(|metric| {
            metric.name == "service.pids" && metric.timestamp.is_none() && metric.value == 3.5
        }));
    }

    #[test]
    fn mysql_slow_log_is_bucketed() {
        let metrics = parse_mysql_slow_series(
            include_str!("../tests/fixtures/mysql-slow-8.0.log"),
            Some((
                "2026-08-27T12:00:00Z".parse().unwrap(),
                "2026-08-27T12:00:05Z".parse().unwrap(),
            )),
        );
        assert!(metrics.iter().any(|metric| {
            metric.name == "db.query.calls" && metric.value == 2.0 && metric.timestamp.is_some()
        }));
        assert!(
            metrics.iter().any(|metric| {
                metric.name == "db.query.total_duration" && metric.value == 350.0
            })
        );
        assert!(metrics.iter().any(|metric| {
            metric.name == "db.query.rows_examined"
                && metric.value == 2.0
                && metric.timestamp.is_some()
        }));
        assert!(metrics.iter().any(|metric| {
            metric.name == "db.query.rows_sent" && metric.value == 2.0 && metric.timestamp.is_some()
        }));
    }

    #[test]
    fn mysql_slow_log_tolerates_binary_query_literals() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mysql-slow.zst");
        let mut encoder =
            zstd::stream::write::Encoder::new(fs::File::create(&path).unwrap(), 1).unwrap();
        use std::io::Write;
        encoder
            .write_all(
                b"# Time: 2026-08-27T12:00:01.000000Z\n# Query_time: 0.001000  Lock_time: 0.000000 Rows_sent: 0 Rows_examined: 0\nINSERT INTO icons(image) VALUES ('\xff\xfe');\n",
            )
            .unwrap();
        encoder.finish().unwrap();

        let metrics = parse_standard_output(&path, CollectorParser::MysqlSlow, None, None)
            .expect("binary literals should be decoded lossily");

        assert!(metrics.iter().any(|metric| metric.name == "db.query.calls"));
    }

    #[test]
    fn mysql_8_0_46_full_slow_log_is_bucketed() {
        let metrics = parse_mysql_slow_series(
            include_str!("../tests/fixtures/mysql-slow-8.0.46-docker.log"),
            Some((
                "2026-08-27T10:42:55Z".parse().unwrap(),
                "2026-08-27T10:42:56Z".parse().unwrap(),
            )),
        );
        let timestamped_calls = metrics
            .iter()
            .filter(|metric| metric.name == "db.query.calls" && metric.timestamp.is_some())
            .map(|metric| metric.value)
            .sum::<f64>();
        let timestamped_duration = metrics
            .iter()
            .filter(|metric| metric.name == "db.query.total_duration" && metric.timestamp.is_some())
            .map(|metric| metric.value)
            .sum::<f64>();
        assert_eq!(timestamped_calls, 10.0);
        assert!((timestamped_duration - 76.788).abs() < 0.001);
        assert!(metrics.iter().any(|metric| {
            metric.timestamp.is_none()
                && metric.labels.get("digest").map(String::as_str)
                    == Some("select * from items where id=?")
        }));
        assert!(
            metrics
                .iter()
                .any(|metric| metric.name == "db.query.p95_duration")
        );
        assert!(
            metrics
                .iter()
                .any(|metric| metric.name == "db.query.lock_duration" && metric.value > 0.0)
        );
    }
}
