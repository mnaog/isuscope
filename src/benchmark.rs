use crate::{
    collector,
    config::{BenchmarkConfig, BenchmarkMode, LoadedConfig},
    model::{BenchmarkResult, LogRef, Metric},
    process,
    shutdown::Shutdown,
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use regex::Regex;
use serde_json::Value;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

pub struct BenchmarkExecution {
    pub result: BenchmarkResult,
    pub logs: Vec<LogRef>,
    pub metrics: Vec<Metric>,
}

#[derive(Default)]
struct Observation {
    score: Option<i64>,
    passed: Option<bool>,
    messages: Vec<String>,
    initialize_started_at: Option<chrono::DateTime<Utc>>,
    initialize_finished_at: Option<chrono::DateTime<Utc>>,
    protocol_result_count: usize,
}

pub async fn execute(
    config: &LoadedConfig,
    run_dir: &Path,
    shutdown: Shutdown,
    collect_metrics: bool,
) -> BenchmarkExecution {
    let started_at = Utc::now();
    let mut execution = match config.config.benchmark.mode {
        BenchmarkMode::Command => {
            match execute_command(config, run_dir, shutdown, collect_metrics).await {
                Ok(execution) => execution,
                Err(error) => BenchmarkExecution {
                    result: BenchmarkResult {
                        mode: "command".into(),
                        command: config.config.benchmark.command.clone(),
                        passed: Some(false),
                        error: Some(format!("{error:#}")),
                        ..Default::default()
                    },
                    logs: Vec::new(),
                    metrics: Vec::new(),
                },
            }
        }
        BenchmarkMode::External => match execute_external(&config.config.benchmark, shutdown).await
        {
            Ok(result) => BenchmarkExecution {
                result,
                logs: Vec::new(),
                metrics: Vec::new(),
            },
            Err(error) => BenchmarkExecution {
                result: BenchmarkResult {
                    mode: "external".into(),
                    passed: Some(false),
                    error: Some(format!("{error:#}")),
                    ..Default::default()
                },
                logs: Vec::new(),
                metrics: Vec::new(),
            },
        },
    };
    execution.result.started_at.get_or_insert(started_at);
    execution.result.finished_at.get_or_insert_with(Utc::now);
    execution
}

async fn execute_command(
    config: &LoadedConfig,
    run_dir: &Path,
    mut shutdown: Shutdown,
    collect_metrics: bool,
) -> Result<BenchmarkExecution> {
    let benchmark = &config.config.benchmark;
    let (program, args) = benchmark
        .command
        .split_first()
        .context("benchmark.command must not be empty in command mode")?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(config.benchmark_working_dir())
        .envs(&benchmark.env)
        .env("ISUSCOPE_BENCHMARK_PROTOCOL", "v1")
        .env("ISUSCOPE_PROJECT_ROOT", &config.project_root)
        .env("ISUSCOPE_RUN_DIR", run_dir)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process::configure_group(&mut command);
    let mut child = command.spawn().with_context(|| {
        format!(
            "cannot start benchmark command `{}`",
            benchmark.command.join(" ")
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .context("benchmark stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("benchmark stderr was not captured")?;
    let score_pattern = Regex::new(&benchmark.score_pattern)
        .context("benchmark.score_pattern is not a valid regular expression")?;
    let observation = Arc::new(Mutex::new(Observation::default()));
    let stdout_raw = run_dir.join("tmp/benchmark-stdout.log");
    let stderr_raw = run_dir.join("tmp/benchmark-stderr.log");
    let stdout_task = tokio::spawn(capture_lines(
        stdout,
        stdout_raw.clone(),
        false,
        observation.clone(),
        score_pattern.clone(),
        benchmark.clone(),
    ));
    let stderr_task = tokio::spawn(capture_lines(
        stderr,
        stderr_raw.clone(),
        true,
        observation.clone(),
        score_pattern,
        benchmark.clone(),
    ));
    let (status, interrupted) = tokio::select! {
        status = child.wait() => {
            (status.context("cannot wait for benchmark process")?, false)
        }
        _ = shutdown.cancelled() => {
            let status = process::terminate_group(&mut child)
                .await
                .context("cannot terminate interrupted benchmark process group")?;
            (status, true)
        }
    };
    stdout_task.await.context("stdout capture task failed")??;
    stderr_task.await.context("stderr capture task failed")??;

    let mut logs = Vec::new();
    compress_log(&stdout_raw, &run_dir.join("logs/benchmark-stdout.zst"))?;
    logs.push(LogRef {
        id: "benchmark-stdout".into(),
        kind: "benchmark-stdout".into(),
        node: None,
    });
    compress_log(&stderr_raw, &run_dir.join("logs/benchmark-stderr.zst"))?;
    logs.push(LogRef {
        id: "benchmark-stderr".into(),
        kind: "benchmark-stderr".into(),
        node: None,
    });
    let mut metrics = if collect_metrics {
        collector::parse_protocol(&run_dir.join("logs/benchmark-stdout.zst"))
            .map(|records| records.0)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    for metric in &mut metrics {
        metric
            .labels
            .insert("isuscope.parser".into(), "inline".into());
    }

    let observation = Arc::try_unwrap(observation)
        .map_err(|_| anyhow::anyhow!("benchmark observation is still shared"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("benchmark observation lock is poisoned"))?;
    let duplicate_result = observation.protocol_result_count > 1;
    let passed = if interrupted || !status.success() || duplicate_result {
        Some(false)
    } else {
        observation
            .passed
            .or(Some(status.success() && observation.score.is_some()))
    };
    Ok(BenchmarkExecution {
        result: BenchmarkResult {
            mode: "command".into(),
            command: benchmark.command.clone(),
            exit_code: status.code(),
            score: observation.score,
            passed,
            interrupted,
            messages: observation.messages,
            started_at: None,
            finished_at: None,
            initialize_started_at: observation.initialize_started_at,
            initialize_finished_at: observation.initialize_finished_at,
            error: if interrupted {
                Some("interrupted by signal".into())
            } else if duplicate_result {
                Some(format!(
                    "benchmark emitted {} isuscope.result records; expected exactly one",
                    observation.protocol_result_count
                ))
            } else if !status.success() {
                Some(format!("benchmark command exited with {status}"))
            } else {
                None
            },
        },
        logs,
        metrics,
    })
}

async fn capture_lines<R>(
    reader: R,
    path: PathBuf,
    stderr: bool,
    observation: Arc<Mutex<Observation>>,
    score_pattern: Regex,
    config: BenchmarkConfig,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    let mut file = tokio::fs::File::create(path).await?;
    while let Some(line) = lines.next_line().await? {
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        if config.stream_output {
            if stderr {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
        observe_line(&line, stderr, &score_pattern, &config, &observation);
    }
    file.flush().await?;
    Ok(())
}

fn observe_line(
    line: &str,
    stderr: bool,
    score_pattern: &Regex,
    config: &BenchmarkConfig,
    observation: &Arc<Mutex<Observation>>,
) {
    let mut observation = match observation.lock() {
        Ok(value) => value,
        Err(_) => return,
    };
    if observation.initialize_started_at.is_none() && line.contains(&config.initialize_start_marker)
    {
        observation.initialize_started_at = Some(Utc::now());
    }
    if observation.initialize_started_at.is_some()
        && observation.initialize_finished_at.is_none()
        && line.contains(&config.initialize_finish_marker)
    {
        observation.initialize_finished_at = Some(Utc::now());
    }
    // Contest benchmarkers commonly log their human-readable score to stderr.
    // The configured regex is safe on both streams; only the structured JSON
    // protocol below is restricted to stdout.
    if let Some(captures) = score_pattern.captures(line)
        && let Some(value) = captures
            .get(1)
            .and_then(|capture| capture.as_str().parse().ok())
    {
        observation.score = Some(value);
    }
    if !stderr && let Ok(value) = serde_json::from_str::<Value>(line) {
        if value.get("type").and_then(Value::as_str) == Some("isuscope.event") {
            match value.get("name").and_then(Value::as_str) {
                Some("initialize-started") if observation.initialize_started_at.is_none() => {
                    observation.initialize_started_at = Some(Utc::now());
                }
                Some("initialize-finished")
                    if observation.initialize_started_at.is_some()
                        && observation.initialize_finished_at.is_none() =>
                {
                    observation.initialize_finished_at = Some(Utc::now());
                }
                _ => {}
            }
        }
        if value.get("type").and_then(Value::as_str) == Some("isuscope.result") {
            observation.protocol_result_count += 1;
            if let Some(score) = value.get("score").and_then(Value::as_i64) {
                observation.score = Some(score);
            }
            if let Some(passed) = value.get("pass").and_then(Value::as_bool) {
                observation.passed = Some(passed);
            }
            if let Some(messages) = value.get("messages").and_then(Value::as_array) {
                observation.messages.extend(
                    messages
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned),
                );
            }
        }
    }
}

async fn execute_external(
    _config: &BenchmarkConfig,
    mut shutdown: Shutdown,
) -> Result<BenchmarkResult> {
    let mut input = BufReader::new(tokio::io::stdin());
    println!("collectors armed; open the contest portal");
    let Some(_) = prompt(
        &mut input,
        &mut shutdown,
        "Press Enter immediately before starting the benchmark: ",
    )
    .await?
    else {
        return Ok(interrupted_external(None));
    };
    let started_at = Utc::now();
    let Some(_) = prompt(
        &mut input,
        &mut shutdown,
        "Press Enter after the benchmark has finished: ",
    )
    .await?
    else {
        return Ok(interrupted_external(Some(started_at)));
    };
    let finished_at = Utc::now();
    let Some(score) = prompt(&mut input, &mut shutdown, "Score (empty if unavailable): ").await?
    else {
        return Ok(interrupted_external(Some(started_at)));
    };
    let score = if score.trim().is_empty() {
        None
    } else {
        Some(score.trim().parse().context("score must be an integer")?)
    };
    let Some(result) = prompt(&mut input, &mut shutdown, "Result [pass/fail]: ").await? else {
        return Ok(interrupted_external(Some(started_at)));
    };
    let passed = match result.trim().to_ascii_lowercase().as_str() {
        "pass" | "p" => Some(true),
        "fail" | "f" => Some(false),
        _ => bail!("result must be `pass` or `fail`"),
    };
    Ok(BenchmarkResult {
        mode: "external".into(),
        score,
        passed,
        started_at: Some(started_at),
        finished_at: Some(finished_at),
        ..Default::default()
    })
}

fn interrupted_external(started_at: Option<chrono::DateTime<Utc>>) -> BenchmarkResult {
    BenchmarkResult {
        mode: "external".into(),
        passed: Some(false),
        interrupted: true,
        started_at,
        finished_at: Some(Utc::now()),
        error: Some("interrupted by signal".into()),
        ..Default::default()
    }
}

async fn prompt<R: tokio::io::AsyncBufRead + Unpin>(
    input: &mut R,
    shutdown: &mut Shutdown,
    message: &str,
) -> Result<Option<String>> {
    print!("{message}");
    io::stdout().flush()?;
    let mut answer = String::new();
    tokio::select! {
        read = input.read_line(&mut answer) => {
            let bytes = read?;
            if bytes == 0 {
                bail!("standard input closed while waiting for external benchmark input");
            }
            Ok(Some(answer))
        }
        _ = shutdown.cancelled() => Ok(None),
    }
}

pub fn compress_log(source: &Path, destination: &Path) -> Result<()> {
    let mut input = fs::File::open(source)?;
    let output = fs::File::create(destination)?;
    zstd::stream::copy_encode(&mut input, output, 3)?;
    fs::remove_file(source)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BenchmarkConfig {
        BenchmarkConfig {
            mode: BenchmarkMode::Command,
            command: vec![],
            working_dir: None,
            env: Default::default(),
            stream_output: false,
            score_pattern: r"スコア:\s*([0-9]+)".into(),
            initialize_start_marker: "初期化を行います".into(),
            initialize_finish_marker: "整合性チェック".into(),
            parsers: Vec::new(),
        }
    }

    #[test]
    fn parses_japanese_score_and_failure_json() {
        let observation = Arc::new(Mutex::new(Observation::default()));
        let regex = Regex::new(&config().score_pattern).unwrap();
        observe_line("スコア: 954885", false, &regex, &config(), &observation);
        observe_line(
            r#"{"type":"isuscope.event","name":"initialize-started"}"#,
            false,
            &regex,
            &config(),
            &observation,
        );
        observe_line(
            r#"{"type":"isuscope.event","name":"initialize-finished"}"#,
            false,
            &regex,
            &config(),
            &observation,
        );
        observe_line(
            r#"{"type":"isuscope.result","pass":false,"score":0,"messages":["initialize failed"]}"#,
            false,
            &regex,
            &config(),
            &observation,
        );
        let value = observation.lock().unwrap();
        assert_eq!(value.score, Some(0));
        assert_eq!(value.passed, Some(false));
        assert_eq!(value.messages, vec!["initialize failed"]);
        assert!(value.initialize_started_at.is_some());
        assert!(value.initialize_finished_at.is_some());
    }

    #[test]
    fn accepts_legacy_stderr_score_but_not_untyped_or_stderr_json() {
        let observation = Arc::new(Mutex::new(Observation::default()));
        let regex = Regex::new(&config().score_pattern).unwrap();
        observe_line("スコア: 3211", true, &regex, &config(), &observation);
        observe_line(
            r#"{"score":9999,"pass":true}"#,
            false,
            &regex,
            &config(),
            &observation,
        );
        observe_line(
            r#"{"type":"isuscope.result","score":8888,"pass":true}"#,
            true,
            &regex,
            &config(),
            &observation,
        );
        let value = observation.lock().unwrap();
        assert_eq!(value.score, Some(3211));
        assert_eq!(value.passed, None);
        assert_eq!(value.protocol_result_count, 0);
    }
}
