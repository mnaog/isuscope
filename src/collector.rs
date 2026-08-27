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
    let script = format!(
        "base=/tmp/isuscope-{run_id}.perf; if sudo -n test -s \"$base.pid\" 2>/dev/null; then pid=$(sudo -n cat \"$base.pid\" 2>/dev/null || true); case $pid in ''|*[!0-9]*) ;; *) sudo -n kill -INT \"$pid\" 2>/dev/null || true ;; esac; fi; sudo -n rm -f \"$base.data\" \"$base.log\" \"$base.pid\" 2>/dev/null || true; find /tmp -maxdepth 1 -type f -name 'isuscope-{run_id}.*' -delete"
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
    let raw = String::from_utf8(bytes).context("stream did not contain valid UTF-8")?;
    match parser {
        CollectorParser::AlpJson => parse_alp_json(&raw, routes),
        CollectorParser::MysqlSlow => unreachable!("handled by streaming parser"),
        CollectorParser::SlpJson => parse_slp_json(&raw),
        CollectorParser::SlpTsv => parse_slp_tsv(&raw),
        CollectorParser::Sysstat => Ok(parse_sysstat(&raw, interval)),
        CollectorParser::PerfScript => parse_perf_script(&raw),
    }
}

fn parse_perf_script(raw: &str) -> Result<Vec<Metric>> {
    let start = raw
        .lines()
        .find_map(|line| line.strip_prefix("# isuscope-perf-start "))
        .and_then(|value| value.trim().parse::<f64>().ok())
        .context("perf script output has no valid isuscope start marker")?;
    let time_pattern = regex::Regex::new(r"(?P<time>[0-9]+(?:[.][0-9]+)?):")?;
    let symbol_pattern = regex::Regex::new(r"(?P<symbol>.+?)\s+\((?P<dso>[^()]*)\)\s*$")?;
    let mut buckets = BTreeMap::<(chrono::DateTime<Utc>, String, String, String), u64>::new();
    let mut parsed_lines = 0_u64;
    for line in raw.lines().filter(|line| !line.starts_with('#')) {
        let Some(time_capture) = time_pattern.captures(line) else {
            continue;
        };
        let Some(time_match) = time_capture.name("time") else {
            continue;
        };
        let Some(symbol_capture) = symbol_pattern.captures(line) else {
            continue;
        };
        let relative = time_match.as_str().parse::<f64>()?;
        let wall = start + relative;
        let seconds = wall.floor() as i64;
        let nanos = ((wall - wall.floor()) * 1_000_000_000.0).round() as u32;
        let Some(at) = chrono::DateTime::from_timestamp(seconds, nanos.min(999_999_999)) else {
            continue;
        };
        let prefix = line[..time_match.start()].trim();
        let process = prefix
            .split_whitespace()
            .take_while(|part| {
                !(part.chars().all(|character| character.is_ascii_digit())
                    || part.starts_with('[') && part.ends_with(']'))
            })
            .collect::<Vec<_>>()
            .join(" ");
        let binary = symbol_capture
            .name("dso")
            .map(|value| value.as_str().trim())
            .unwrap_or("-");
        let raw_symbol = symbol_capture
            .name("symbol")
            .map(|value| value.as_str().trim())
            .unwrap_or("-");
        let after_event = raw_symbol
            .rsplit_once(": ")
            .map_or(raw_symbol, |(_, value)| value)
            .trim();
        let mut symbol_fields = after_event.splitn(2, char::is_whitespace);
        let first = symbol_fields.next().unwrap_or("-");
        let symbol =
            if !first.is_empty() && first.chars().all(|character| character.is_ascii_hexdigit()) {
                symbol_fields.next().unwrap_or("-").trim()
            } else {
                after_event
            };
        let Some(bucket) = chrono::DateTime::from_timestamp(at.timestamp() / 5 * 5, 0) else {
            continue;
        };
        *buckets
            .entry((
                bucket,
                if process.is_empty() { "-" } else { &process }.into(),
                canonical_perf_binary(binary),
                canonical_perf_symbol(symbol),
            ))
            .or_default() += 1;
        parsed_lines += 1;
    }
    if raw
        .lines()
        .any(|line| !line.starts_with('#') && !line.trim().is_empty())
        && parsed_lines == 0
    {
        anyhow::bail!("perf script output contained samples but none matched the supported format");
    }
    let mut totals = BTreeMap::<chrono::DateTime<Utc>, u64>::new();
    for ((at, _, _, _), count) in &buckets {
        *totals.entry(*at).or_default() += count;
    }
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
        query: Vec<String>,
    }
    #[derive(Default)]
    struct Stats {
        calls: u64,
        total_ms: f64,
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
        }
        let stats = aggregate.entry(digest).or_default();
        stats.calls += 1;
        stats.total_ms += duration_ms;
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
            event.duration_ms = rest
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<f64>().ok())
                .map(|seconds| seconds * 1_000.0);
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
        } else if character.is_ascii_digit() {
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
    let mut routes = BTreeMap::<(String, String), (f64, Option<f64>)>::new();
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
        if let Some(count) = number(object, &["count", "requests"]) {
            stats.0 += count;
        }
        if let Some(p95_seconds) = number(object, &["p95", "p95_time", "request_time_p95"]) {
            stats.1 = Some(
                stats
                    .1
                    .map_or(p95_seconds, |current| current.max(p95_seconds)),
            );
        }
    }
    let mut metrics = Vec::new();
    for ((method, route), (requests, p95_seconds)) in routes {
        let labels = BTreeMap::from([("method".into(), method), ("route".into(), route)]);
        if requests > 0.0 {
            metrics.push(Metric {
                name: "http.requests".into(),
                value: requests,
                unit: "requests".into(),
                timestamp: None,
                labels: labels.clone(),
            });
        }
        if let Some(p95_seconds) = p95_seconds {
            let mut labels = labels;
            labels.insert("quantile".into(), "0.95".into());
            metrics.push(Metric {
                name: "http.request_duration".into(),
                value: p95_seconds * 1000.0,
                unit: "ms".into(),
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

fn parse_sysstat(
    raw: &str,
    interval: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
) -> Vec<Metric> {
    let mut cpu_header: Option<Vec<String>> = None;
    let mut disk_header: Option<Vec<String>> = None;
    let mut cpu = (0.0_f64, 0_u64);
    let mut disks = BTreeMap::<String, (f64, u64, f64, u64)>::new();
    let date = regex::Regex::new(r"\b(\d{2}/\d{2}/\d{2})\b")
        .ok()
        .and_then(|pattern| pattern.captures(raw))
        .and_then(|capture| NaiveDate::parse_from_str(&capture[1], "%m/%d/%y").ok());
    let mut metrics = Vec::new();
    for line in raw.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.contains(&"CPU") && fields.contains(&"%idle") {
            cpu_header = Some(fields.iter().map(|field| field.to_string()).collect());
            continue;
        }
        if let Some(header) = &cpu_header {
            let cpu_index = header.iter().position(|field| field == "CPU");
            let idle_index = header.iter().position(|field| field == "%idle");
            if fields.len() == header.len()
                && cpu_index.is_some_and(|index| fields[index] == "all")
                && let Some(idle) =
                    idle_index.and_then(|index| fields[index].replace(',', ".").parse::<f64>().ok())
            {
                let value = 100.0 - idle;
                let timestamp = sysstat_timestamp(date, &fields);
                if !within_interval(timestamp, interval) {
                    continue;
                }
                cpu.0 += value;
                cpu.1 += 1;
                if let Some(timestamp) = timestamp {
                    metrics.push(Metric {
                        name: "host.cpu_percent".into(),
                        value,
                        unit: "percent".into(),
                        timestamp: Some(timestamp),
                        labels: BTreeMap::new(),
                    });
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
        let values = disks.entry(device.clone()).or_default();
        if let Some(value) = header
            .iter()
            .position(|field| field == "await")
            .and_then(|index| fields[index].replace(',', ".").parse::<f64>().ok())
        {
            let timestamp = sysstat_timestamp(date, &fields);
            if !within_interval(timestamp, interval) {
                continue;
            }
            values.0 += value;
            values.1 += 1;
            if let Some(timestamp) = timestamp {
                metrics.push(Metric {
                    name: "host.disk_await".into(),
                    value,
                    unit: "ms".into(),
                    timestamp: Some(timestamp),
                    labels: BTreeMap::from([("device".into(), device.clone())]),
                });
            }
        }
        if let Some(value) = header
            .iter()
            .position(|field| field == "%util")
            .and_then(|index| fields[index].replace(',', ".").parse::<f64>().ok())
        {
            let timestamp = sysstat_timestamp(date, &fields);
            if !within_interval(timestamp, interval) {
                continue;
            }
            values.2 += value;
            values.3 += 1;
            if let Some(timestamp) = timestamp {
                metrics.push(Metric {
                    name: "host.disk_util_percent".into(),
                    value,
                    unit: "percent".into(),
                    timestamp: Some(timestamp),
                    labels: BTreeMap::from([("device".into(), device.clone())]),
                });
            }
        }
    }
    if cpu.1 > 0 {
        metrics.push(Metric {
            name: "host.cpu_percent".into(),
            value: cpu.0 / cpu.1 as f64,
            unit: "percent".into(),
            timestamp: None,
            labels: BTreeMap::new(),
        });
    }
    for (device, (await_sum, await_count, util_sum, util_count)) in disks {
        let labels = BTreeMap::from([("device".into(), device)]);
        if await_count > 0 {
            metrics.push(Metric {
                name: "host.disk_await".into(),
                value: await_sum / await_count as f64,
                unit: "ms".into(),
                timestamp: None,
                labels: labels.clone(),
            });
        }
        if util_count > 0 {
            metrics.push(Metric {
                name: "host.disk_util_percent".into(),
                value: util_sum / util_count as f64,
                unit: "percent".into(),
                timestamp: None,
                labels,
            });
        }
    }
    metrics
}

fn within_interval(
    timestamp: Option<chrono::DateTime<Utc>>,
    interval: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
) -> bool {
    interval.is_none_or(|(start, end)| timestamp.is_some_and(|at| at >= start && at <= end))
}

fn sysstat_timestamp(date: Option<NaiveDate>, fields: &[&str]) -> Option<chrono::DateTime<Utc>> {
    let value = match fields.get(1).copied() {
        Some("AM" | "PM") => format!("{} {}", fields.first()?, fields[1]),
        _ => fields.first()?.to_string(),
    };
    let time = NaiveTime::parse_from_str(&value, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(&value, "%I:%M:%S %p"))
        .ok()?;
    Some(Utc.from_utc_datetime(&NaiveDateTime::new(date?, time)))
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
            )
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
    route_matching_groups: Option<&str>,
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
    fn standard_json_adapters_emit_bottleneck_metrics() {
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
    fn current_alp_table_json_and_slp_tsv_emit_bottleneck_metrics() {
        let alp = parse_alp_json(
            include_str!("../tests/fixtures/alp-json-v1.0.21.json"),
            None,
        )
        .unwrap();
        assert!(alp.iter().any(|metric| {
            metric.name == "http.requests"
                && metric.value == 1.0
                && metric.labels.get("route").map(String::as_str) == Some("/api/users/1")
        }));
        assert!(
            alp.iter()
                .any(|metric| { metric.name == "http.request_duration" && metric.value == 200.0 })
        );
        assert!(
            parse_alp_json(
                include_str!("../tests/fixtures/alp-json-v1.0.21-empty.json"),
                None,
            )
            .unwrap()
            .is_empty()
        );
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
    }
}
