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

fn parse_standard_output(
    path: &Path,
    parser: CollectorParser,
    routes: Option<&Path>,
    interval: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
) -> Result<Vec<Metric>> {
    let input = fs::File::open(path)?;
    let mut decoder = zstd::stream::read::Decoder::new(input)?;
    let mut raw = String::new();
    use std::io::Read;
    decoder.read_to_string(&mut raw)?;
    match parser {
        CollectorParser::AlpJson => parse_alp_json(&raw, routes),
        CollectorParser::MysqlSlow => Ok(parse_mysql_slow_series(&raw, interval)),
        CollectorParser::SlpJson => parse_slp_json(&raw),
        CollectorParser::Sysstat => Ok(parse_sysstat(&raw, interval)),
    }
}

fn benchmark_interval(run_dir: &Path) -> Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
    let manifest: crate::model::RunManifest =
        serde_json::from_slice(&fs::read(run_dir.join("run.json")).ok()?).ok()?;
    Some((
        manifest.benchmark.started_at?,
        manifest.benchmark.finished_at?,
    ))
}

fn parse_mysql_slow_series(
    raw: &str,
    interval: Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)>,
) -> Vec<Metric> {
    let mut timestamp = None;
    let mut buckets = BTreeMap::<chrono::DateTime<Utc>, (u64, f64)>::new();
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("# Time: ") {
            timestamp = chrono::DateTime::parse_from_rfc3339(value.trim())
                .ok()
                .map(|value| value.with_timezone(&Utc));
        } else if let Some(rest) = line.strip_prefix("# Query_time: ")
            && let (Some(at), Some(seconds)) = (
                timestamp,
                rest.split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<f64>().ok()),
            )
            && interval.is_none_or(|(start, end)| at >= start && at <= end)
            && let Some(bucket) = chrono::DateTime::from_timestamp(at.timestamp() / 5 * 5, 0)
        {
            let values = buckets.entry(bucket).or_default();
            values.0 += 1;
            values.1 += seconds * 1_000.0;
        }
    }
    let labels = BTreeMap::from([
        ("engine".into(), "mysql".into()),
        ("digest".into(), "all".into()),
    ]);
    buckets
        .into_iter()
        .flat_map(|(timestamp, (calls, duration))| {
            [
                Metric {
                    name: "db.query.calls".into(),
                    value: calls as f64,
                    unit: "queries".into(),
                    timestamp: Some(timestamp),
                    labels: labels.clone(),
                },
                Metric {
                    name: "db.query.total_duration".into(),
                    value: duration,
                    unit: "ms".into(),
                    timestamp: Some(timestamp),
                    labels: labels.clone(),
                },
            ]
        })
        .collect()
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
    let records = json_records(raw)?;
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
}
