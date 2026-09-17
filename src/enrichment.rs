use crate::{
    benchmark::compress_log,
    collector::{capture_capped, parse_protocol, sanitize},
    config::{BenchmarkParserConfig, LoadedConfig},
    model::{BenchmarkMessage, BenchmarkMessageKind, EnrichmentResult, LogRef, Metric},
    process,
    storage::Store,
    tooling,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;
use uuid::Uuid;

pub const PARSER_LABEL: &str = "isuscope.parser";

/// Failure messages kept per parser. The first lines usually name the cause.
const MAX_FAILURE_MESSAGES: usize = 10;
/// Error samples kept per category, and categories kept per parser.
const MAX_ERROR_SAMPLES_PER_CATEGORY: usize = 5;
const MAX_ERROR_CATEGORIES: usize = 20;
const MAX_MESSAGE_CHARS: usize = 1000;

pub struct EnrichmentOutput {
    pub result: EnrichmentResult,
    pub logs: Vec<LogRef>,
    pub metrics: Vec<Metric>,
}

pub struct EnrichOutcome {
    pub run_id: String,
    pub parser_count: usize,
    pub metric_count: usize,
    pub failed: bool,
}

pub async fn enrich_saved(config: &LoadedConfig, requested: &str) -> Result<EnrichOutcome> {
    if config.config.benchmark.parsers.is_empty() {
        anyhow::bail!("no benchmark parsers are configured; add [[benchmark.parsers]] first");
    }
    let mut store = Store::open(&config.data_dir)?;
    let id = store
        .resolve_id(requested)?
        .with_context(|| format!("run `{requested}` was not found"))?;
    let mut manifest = store.load(&id)?;
    let run_dir = store.final_dir(&id);
    if !run_dir.is_dir() {
        anyhow::bail!("run `{requested}` is not finalized");
    }
    let enrichment_id = Uuid::now_v7().to_string();
    let relative_tooling = format!("tooling/enrichments/{enrichment_id}");
    if let Err(error) = tooling::capture(config, &run_dir.join(&relative_tooling)) {
        eprintln!("! cannot snapshot enrichment tooling: {error:#}");
    }
    let mut outputs = run_all(config, &id, &run_dir).await;
    for output in &mut outputs {
        output.result.tooling_path = Some(relative_tooling.clone());
    }
    let failed = outputs
        .iter()
        .any(|output| output.result.status == "failed");
    let parser_count = outputs.len();
    store.replace_enrichments(&mut manifest, outputs)?;
    Ok(EnrichOutcome {
        run_id: id,
        parser_count,
        metric_count: manifest.metric_count,
        failed,
    })
}

pub async fn run_all(config: &LoadedConfig, run_id: &str, run_dir: &Path) -> Vec<EnrichmentOutput> {
    let mut outputs = Vec::new();
    for parser in &config.config.benchmark.parsers {
        outputs.push(run_one(config, parser, run_id, run_dir).await);
    }
    outputs
}

async fn run_one(
    config: &LoadedConfig,
    parser: &BenchmarkParserConfig,
    run_id: &str,
    run_dir: &Path,
) -> EnrichmentOutput {
    match execute(config, parser, run_id, run_dir).await {
        Ok(output) => output,
        Err(error) => EnrichmentOutput {
            result: EnrichmentResult {
                name: parser.name.clone(),
                status: "failed".into(),
                command: parser.command.clone(),
                exit_code: None,
                error: Some(format!("{error:#}")),
                log_ids: Vec::new(),
                tooling_path: None,
                messages: Vec::new(),
                omitted_message_count: 0,
            },
            logs: Vec::new(),
            metrics: Vec::new(),
        },
    }
}

async fn execute(
    config: &LoadedConfig,
    parser: &BenchmarkParserConfig,
    run_id: &str,
    run_dir: &Path,
) -> Result<EnrichmentOutput> {
    let stdout_log = run_dir.join("logs/benchmark-stdout.zst");
    let stderr_log = run_dir.join("logs/benchmark-stderr.zst");
    if !stdout_log.is_file() {
        anyhow::bail!("benchmark stdout is missing: {}", stdout_log.display());
    }
    let expanded = parser
        .command
        .iter()
        .map(|argument| {
            argument
                .replace("{run_id}", run_id)
                .replace("{run_dir}", &run_dir.display().to_string())
                .replace("{benchmark_stdout}", &stdout_log.display().to_string())
                .replace("{benchmark_stderr}", &stderr_log.display().to_string())
        })
        .collect::<Vec<_>>();
    let (program, args) = expanded
        .split_first()
        .context("benchmark parser command must not be empty")?;
    let prefix = format!("benchmark-parser-{}", sanitize(&parser.name));
    let stdout_raw = run_dir.join("tmp").join(format!("{prefix}-stdout.log"));
    let stderr_raw = run_dir.join("tmp").join(format!("{prefix}-stderr.log"));
    std::fs::create_dir_all(run_dir.join("tmp"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&config.project_root)
        .env("ISUSCOPE_BENCHMARK_PROTOCOL", "v1")
        .env("ISUSCOPE_PROJECT_ROOT", &config.project_root)
        .env("ISUSCOPE_RUN_DIR", run_dir)
        .env("ISUSCOPE_BENCHMARK_STDOUT", &stdout_log)
        .env("ISUSCOPE_BENCHMARK_STDERR", &stderr_log)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process::configure_group(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start benchmark parser `{}`", parser.name))?;
    let stdout = child
        .stdout
        .take()
        .context("parser stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("parser stderr was not captured")?;
    let stdout_task = tokio::spawn(capture_capped(
        stdout,
        stdout_raw.clone(),
        parser.max_output_bytes,
    ));
    let stderr_task = tokio::spawn(capture_capped(
        stderr,
        stderr_raw.clone(),
        parser.max_output_bytes,
    ));
    let waited =
        tokio::time::timeout(Duration::from_secs(parser.timeout_seconds), child.wait()).await;
    let (exit_code, mut errors) = match waited {
        Ok(Ok(status)) => (status.code(), Vec::new()),
        Ok(Err(error)) => (None, vec![format!("cannot wait for parser: {error}")]),
        Err(_) => {
            let _ = process::terminate_group(&mut child).await;
            (
                None,
                vec![format!(
                    "benchmark parser timed out after {}s",
                    parser.timeout_seconds
                )],
            )
        }
    };
    match stdout_task.await {
        Ok(Ok(true)) => errors.push(format!(
            "stdout truncated at {} bytes",
            parser.max_output_bytes
        )),
        Ok(Ok(false)) => {}
        Ok(Err(error)) => errors.push(format!("stdout capture failed: {error}")),
        Err(error) => errors.push(format!("stdout capture task failed: {error}")),
    }
    match stderr_task.await {
        Ok(Ok(true)) => errors.push(format!(
            "stderr truncated at {} bytes",
            parser.max_output_bytes
        )),
        Ok(Ok(false)) => {}
        Ok(Err(error)) => errors.push(format!("stderr capture failed: {error}")),
        Err(error) => errors.push(format!("stderr capture task failed: {error}")),
    }
    if exit_code != Some(0) && errors.is_empty() {
        errors.push(format!(
            "benchmark parser exited with {}",
            exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "no status".into())
        ));
    }

    let stdout_id = format!("{prefix}-stdout");
    let stderr_id = format!("{prefix}-stderr");
    let stdout_destination = run_dir.join("logs").join(format!("{stdout_id}.zst"));
    let stderr_destination = run_dir.join("logs").join(format!("{stderr_id}.zst"));
    let mut logs = Vec::new();
    if let Err(error) = compress_log(&stdout_raw, &stdout_destination) {
        errors.push(format!("cannot save parser stdout: {error}"));
    } else {
        logs.push(LogRef {
            id: stdout_id.clone(),
            kind: format!("benchmark-parser:{}:stdout", parser.name),
            node: None,
        });
    }
    if let Err(error) = compress_log(&stderr_raw, &stderr_destination) {
        errors.push(format!("cannot save parser stderr: {error}"));
    } else {
        logs.push(LogRef {
            id: stderr_id.clone(),
            kind: format!("benchmark-parser:{}:stderr", parser.name),
            node: None,
        });
    }

    let mut metrics = if stdout_destination.is_file() {
        match parse_protocol(&stdout_destination) {
            Ok((metrics, fingerprints, transitions)) => {
                if !fingerprints.is_empty() || !transitions.is_empty() {
                    errors.push(
                        "benchmark parsers may emit metric and message records only; fingerprint and transition records were ignored"
                            .into(),
                    );
                }
                metrics
            }
            Err(error) => {
                errors.push(format!("cannot parse benchmark parser output: {error:#}"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    for metric in &mut metrics {
        metric
            .labels
            .insert(PARSER_LABEL.into(), parser.name.clone());
    }
    let (messages, omitted_message_count) = if stdout_destination.is_file() {
        match parse_messages(&stdout_destination) {
            Ok(parsed) => {
                if parsed.invalid > 0 {
                    errors.push(format!(
                        "{} message records were ignored; kind must be `failure` or `error` and text must not be empty",
                        parsed.invalid
                    ));
                }
                (parsed.messages, parsed.omitted)
            }
            Err(error) => {
                errors.push(format!("cannot parse benchmark parser messages: {error:#}"));
                (Vec::new(), 0)
            }
        }
    } else {
        (Vec::new(), 0)
    };
    let success = errors.is_empty();
    Ok(EnrichmentOutput {
        result: EnrichmentResult {
            name: parser.name.clone(),
            status: if success { "complete" } else { "failed" }.into(),
            command: expanded,
            exit_code,
            error: (!errors.is_empty()).then(|| errors.join("; ")),
            log_ids: logs.iter().map(|log| log.id.clone()).collect(),
            tooling_path: None,
            messages,
            omitted_message_count,
        },
        logs,
        metrics,
    })
}

struct ParsedMessages {
    messages: Vec<BenchmarkMessage>,
    omitted: usize,
    invalid: usize,
}

/// Reads `{"type":"message","kind":"failure"|"error","category":...,"text":...}` records.
/// Failures keep the first lines; errors keep the first samples of each category.
fn parse_messages(path: &Path) -> Result<ParsedMessages> {
    let decoder = zstd::stream::read::Decoder::new(std::fs::File::open(path)?)?;
    let mut parsed = ParsedMessages {
        messages: Vec::new(),
        omitted: 0,
        invalid: 0,
    };
    let mut failures = 0;
    let mut error_samples = BTreeMap::<Option<String>, usize>::new();
    for line in BufReader::new(decoder).split(b'\n').map_while(Result::ok) {
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let kind = match value.get("kind").and_then(Value::as_str) {
            Some("failure") => BenchmarkMessageKind::Failure,
            Some("error") => BenchmarkMessageKind::Error,
            _ => {
                parsed.invalid += 1;
                continue;
            }
        };
        let Some(text) = value
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            parsed.invalid += 1;
            continue;
        };
        let category = value
            .get("category")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|category| !category.is_empty())
            .map(|category| truncate_chars(category, 200));
        let kept = match kind {
            BenchmarkMessageKind::Failure => {
                failures += 1;
                failures <= MAX_FAILURE_MESSAGES
            }
            BenchmarkMessageKind::Error => {
                let known = error_samples.len();
                match error_samples.get_mut(&category) {
                    Some(count) => {
                        *count += 1;
                        *count <= MAX_ERROR_SAMPLES_PER_CATEGORY
                    }
                    None if known < MAX_ERROR_CATEGORIES => {
                        error_samples.insert(category.clone(), 1);
                        true
                    }
                    None => false,
                }
            }
        };
        if kept {
            parsed.messages.push(BenchmarkMessage {
                kind,
                category,
                text: truncate_chars(text, MAX_MESSAGE_CHARS),
            });
        } else {
            parsed.omitted += 1;
        }
    }
    Ok(parsed)
}

fn truncate_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}
