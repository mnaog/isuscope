//! Commands around the benchmark itself: `[benchmark] prepare_command`, which runs before every
//! collector, and `[benchmark] rescore_command`, which `isuscope rescore` uses to judge a run
//! whose result the adapter left pending.
use crate::{
    benchmark::compress_log,
    collector::capture_capped,
    config::LoadedConfig,
    model::{AnalysisStatus, LogRef, RunManifest},
    process,
    storage::Store,
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

const OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;
const RESCORE_TIMEOUT: Duration = Duration::from_secs(60);

struct Finished {
    error: Option<String>,
    stdout: PathBuf,
    stderr: PathBuf,
}

/// Runs a configured command with the run's placeholders and captures its output under
/// `<run_dir>/tmp`. The error names the command, its exit and the last stderr line.
async fn run_command(
    config: &LoadedConfig,
    label: &str,
    command: &[String],
    run_id: &str,
    run_dir: &Path,
    timeout: Duration,
) -> Result<Finished> {
    let stdout_log = run_dir.join("logs/benchmark-stdout.zst");
    let stderr_log = run_dir.join("logs/benchmark-stderr.zst");
    let expanded = command
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
        .with_context(|| format!("{label} must not be empty"))?;
    std::fs::create_dir_all(run_dir.join("tmp"))?;
    let stdout = run_dir.join("tmp").join(format!("{label}-stdout.log"));
    let stderr = run_dir.join("tmp").join(format!("{label}-stderr.log"));
    let mut child = Command::new(program);
    child
        .args(args)
        .current_dir(&config.project_root)
        .env("ISUSCOPE_PROJECT_ROOT", &config.project_root)
        .env("ISUSCOPE_RUN_DIR", run_dir)
        .env("ISUSCOPE_RUN_ID", run_id)
        .env("ISUSCOPE_BENCHMARK_STDOUT", &stdout_log)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process::configure_group(&mut child);
    let mut child = child
        .spawn()
        .with_context(|| format!("cannot start {label} `{program}`"))?;
    let group = child.id();
    let stdout_task = tokio::spawn(capture_capped(
        child.stdout.take().context("stdout was not captured")?,
        stdout.clone(),
        OUTPUT_LIMIT,
    ));
    let stderr_task = tokio::spawn(capture_capped(
        child.stderr.take().context("stderr was not captured")?,
        stderr.clone(),
        OUTPUT_LIMIT,
    ));
    let mut error = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => None,
        Ok(Ok(status)) => Some(format!("{label} exited with {status}")),
        Ok(Err(error)) => Some(format!("cannot wait for {label}: {error}")),
        Err(_) => {
            let _ = process::terminate_group(&mut child).await;
            Some(format!("{label} timed out after {}s", timeout.as_secs()))
        }
    };
    for task in [stdout_task, stderr_task] {
        let _ = process::finish_capture(task, process::CAPTURE_DEADLINE, group).await;
    }
    if let Some(message) = &mut error {
        let tail = std::fs::read_to_string(&stderr).unwrap_or_default();
        if let Some(line) = tail.lines().rev().find(|line| !line.trim().is_empty()) {
            message.push_str(": ");
            message.push_str(line.trim());
        }
    }
    Ok(Finished {
        error,
        stdout,
        stderr,
    })
}

/// Runs `[benchmark] prepare_command` before any collector. Its output is kept as the
/// `benchmark-prepare` logs; the returned error means the benchmark must not start.
pub async fn prepare(
    config: &LoadedConfig,
    run_id: &str,
    run_dir: &Path,
    logs: &mut Vec<LogRef>,
) -> Option<String> {
    let benchmark = &config.config.benchmark;
    let finished = match run_command(
        config,
        "benchmark-prepare",
        &benchmark.prepare_command,
        run_id,
        run_dir,
        Duration::from_secs(benchmark.prepare_timeout_seconds),
    )
    .await
    {
        Ok(finished) => finished,
        Err(error) => return Some(format!("{error:#}")),
    };
    for (raw, stream) in [(&finished.stdout, "stdout"), (&finished.stderr, "stderr")] {
        let id = format!("benchmark-prepare-{stream}");
        if compress_log(raw, &run_dir.join("logs").join(format!("{id}.zst"))).is_ok() {
            logs.push(LogRef {
                id,
                kind: format!("benchmark-prepare:{stream}"),
                node: None,
            });
        }
    }
    finished.error
}

pub struct RescoreOutcome {
    pub manifest: RunManifest,
}

/// Judges a pending run with `[benchmark] rescore_command` on its saved stdout. Only a run the
/// adapter left pending can be rescored, so a recorded verdict is never replaced.
pub async fn rescore(config: &LoadedConfig, requested: &str) -> Result<RescoreOutcome> {
    let command = &config.config.benchmark.rescore_command;
    if command.is_empty() {
        bail!(
            "[benchmark] rescore_command is not configured; set it to a command that reads {{benchmark_stdout}} and prints one isuscope.result"
        );
    }
    let mut store = Store::open(&config.data_dir)?;
    let id = store.require_id(requested, "run")?;
    let lock = store.lock_run(&id)?;
    let mut manifest = store.load(&id)?;
    if !manifest.benchmark.pending {
        bail!(
            "run {} already has a benchmark result; rescore only judges runs the adapter left pending",
            crate::runner::short_id(&id)
        );
    }
    let run_dir = store.final_dir(&id);
    let finished = run_command(
        config,
        "benchmark-rescore",
        command,
        &id,
        &run_dir,
        RESCORE_TIMEOUT,
    )
    .await?;
    let output = std::fs::read_to_string(&finished.stdout).unwrap_or_default();
    let _ = std::fs::remove_file(&finished.stdout);
    let _ = std::fs::remove_file(&finished.stderr);
    if let Some(error) = finished.error {
        bail!("{error}");
    }
    let results = output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("isuscope.result"))
        .collect::<Vec<_>>();
    let [result] = &results[..] else {
        bail!(
            "rescore_command printed {} isuscope.result records; expected exactly one",
            results.len()
        );
    };
    if result.get("pending").and_then(Value::as_bool) == Some(true) {
        bail!(
            "rescore_command still cannot judge the output; fix the adapter's sample and patterns first"
        );
    }
    let passed = result
        .get("pass")
        .and_then(Value::as_bool)
        .context("rescore_command printed no `pass` in its isuscope.result")?;
    let benchmark = &mut manifest.benchmark;
    benchmark.passed = Some(passed);
    benchmark.score = result.get("score").and_then(Value::as_i64);
    benchmark.pending = false;
    benchmark.error = None;
    benchmark.messages.push(format!(
        "result set by `isuscope rescore` at {} from the saved stdout",
        crate::model::display_time(Utc::now())
    ));
    if let Some(messages) = result.get("messages").and_then(Value::as_array) {
        benchmark.messages.extend(
            messages
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned),
        );
    }
    manifest.settle_state();
    manifest.analysis_status = if passed {
        AnalysisStatus::Pending
    } else {
        AnalysisStatus::NotRequired
    };
    store.set_result(&lock, &manifest)?;
    Ok(RescoreOutcome { manifest })
}
