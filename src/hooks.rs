//! `[benchmark]`の`prepare_command`・`identity_command`・`cleanup_command`を動かす。
//!
//! どれも手元で、project rootをcurrent directoryにして動かす。環境変数はisuscopeを起動した
//! shellのものをそのまま引き継ぎ（`FOO=... isuscope run`のような指定もそのまま届く）、runの情報を
//! `ISUSCOPE_*`で足す。stdoutとstderrはrunのlogへ残す。

use crate::{
    benchmark::compress_log,
    collector::capture_capped,
    config::LoadedConfig,
    model::{HookResult, LogRef, RunMode},
    process,
    shutdown::Shutdown,
};
use anyhow::{Context, Result};
use chrono::Utc;
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::process::Command;

/// hookの出力として残す上限。identityの比較に使うのは先頭だけで足りる。
const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;

pub struct HookOutput {
    pub result: HookResult,
    pub logs: Vec<LogRef>,
    /// 取り込んだstdout（上限まで）。`identity_command`の値に使う。
    pub stdout: Vec<u8>,
}

pub struct HookContext<'a> {
    pub config: &'a LoadedConfig,
    pub run_id: &'a str,
    pub run_dir: &'a Path,
    pub mode: RunMode,
    /// `ISUSCOPE_*`のほかに渡す環境変数。
    pub env: Vec<(&'static str, String)>,
    /// signalを受けたら、期限を待たずにhookを止める。
    pub shutdown: Option<Shutdown>,
}

pub async fn run(context: &HookContext<'_>, name: &str, command: &[String]) -> HookOutput {
    let started_at = Utc::now();
    let started = Instant::now();
    let mut output = match execute(context, name, command).await {
        Ok(output) => output,
        Err(error) => HookOutput {
            result: HookResult {
                name: name.into(),
                command: command.to_vec(),
                status: "failed".into(),
                error: Some(format!("{error:#}")),
                ..Default::default()
            },
            logs: Vec::new(),
            stdout: Vec::new(),
        },
    };
    output.result.started_at = Some(started_at);
    output.result.duration_ms = Some(started.elapsed().as_millis() as u64);
    output
}

async fn execute(context: &HookContext<'_>, name: &str, command: &[String]) -> Result<HookOutput> {
    let expanded = command
        .iter()
        .map(|argument| {
            argument
                .replace("{run_id}", context.run_id)
                .replace("{run_dir}", &context.run_dir.display().to_string())
        })
        .collect::<Vec<_>>();
    let (program, args) = expanded
        .split_first()
        .with_context(|| format!("[benchmark] {name}_command must not be empty"))?;
    let tmp = context.run_dir.join("tmp");
    std::fs::create_dir_all(&tmp)?;
    let prefix = format!("benchmark-{name}");
    let stdout_raw = tmp.join(format!("{prefix}-stdout.log"));
    let stderr_raw = tmp.join(format!("{prefix}-stderr.log"));
    let mut child = Command::new(program);
    child
        .args(args)
        .current_dir(&context.config.project_root)
        .env("ISUSCOPE_PROJECT_ROOT", &context.config.project_root)
        .env("ISUSCOPE_RUN_DIR", context.run_dir)
        .env("ISUSCOPE_RUN_ID", context.run_id)
        .env("ISUSCOPE_RUN_MODE", context.mode.as_str())
        .envs(context.env.iter().map(|(key, value)| (*key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    process::configure_group(&mut child);
    let mut child = child
        .spawn()
        .with_context(|| format!("cannot start {name}_command `{}`", expanded.join(" ")))?;
    let group = child.id();
    let _registered = process::register_group(group);
    let stdout = child
        .stdout
        .take()
        .context("hook stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("hook stderr was not captured")?;
    let stdout_task = tokio::spawn(capture_capped(stdout, stdout_raw.clone(), MAX_OUTPUT_BYTES));
    let stderr_task = tokio::spawn(capture_capped(stderr, stderr_raw.clone(), MAX_OUTPUT_BYTES));
    let limit = Duration::from_secs(context.config.config.benchmark.hook_timeout_seconds);
    let mut shutdown = context.shutdown.clone();
    let waited = tokio::select! {
        waited = tokio::time::timeout(limit, child.wait()) => Some(waited),
        _ = async {
            match shutdown.as_mut() {
                Some(shutdown) => shutdown.cancelled().await,
                None => std::future::pending().await,
            }
        } => None,
    };
    let (exit_code, mut errors) = match waited {
        Some(Ok(Ok(status))) => (
            status.code(),
            if status.success() {
                Vec::new()
            } else {
                vec![format!("{name}_command exited with {status}")]
            },
        ),
        Some(Ok(Err(error))) => (None, vec![error.to_string()]),
        Some(Err(_)) => {
            let _ = process::terminate_group(&mut child).await;
            (
                None,
                vec![format!(
                    "{name}_command did not finish within [benchmark] hook_timeout_seconds ({}s)",
                    limit.as_secs()
                )],
            )
        }
        None => {
            let _ = process::terminate_group(&mut child).await;
            (None, vec![format!("{name}_command interrupted by signal")])
        }
    };
    // A background process that keeps the pipes open is not the command failing: when the
    // command itself succeeded, the cut-off log is only a warning.
    let mut warnings = Vec::new();
    let captures =
        process::finish_captures([stdout_task, stderr_task], process::CAPTURE_DEADLINE, group)
            .await;
    for (capture, stream) in captures.into_iter().zip(["stdout", "stderr"]) {
        match capture {
            process::Capture::Finished(Ok(Ok(_))) => {}
            process::Capture::Finished(Ok(Err(error))) => {
                errors.push(format!("{stream} capture failed: {error}"))
            }
            process::Capture::Finished(Err(error)) => {
                errors.push(format!("{stream} capture task failed: {error}"))
            }
            process::Capture::Abandoned => warnings.push(format!(
                "{stream} stayed open for {}s after {name}_command exited (a background process held it); the saved log may be incomplete",
                process::CAPTURE_DEADLINE.as_secs()
            )),
        }
    }
    let stdout = std::fs::read(&stdout_raw).unwrap_or_default();
    // 失敗の理由はstderrの末尾にあることが多いので、結果のerrorへ添える。
    if !errors.is_empty() {
        let stderr = std::fs::read(&stderr_raw).unwrap_or_default();
        let tail = String::from_utf8_lossy(&stderr);
        let tail = tail
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        let tail = tail[tail.len().saturating_sub(3)..].join(" | ");
        if !tail.is_empty() {
            errors.push(tail);
        }
    }
    let mut logs = Vec::new();
    for (raw, stream) in [(&stdout_raw, "stdout"), (&stderr_raw, "stderr")] {
        let id = format!("{prefix}-{stream}");
        match compress_log(raw, &context.run_dir.join("logs").join(format!("{id}.zst"))) {
            Ok(()) => logs.push(LogRef {
                id,
                kind: format!("benchmark-{name}:{stream}"),
                node: None,
            }),
            Err(error) => errors.push(format!("cannot save {name}_command {stream}: {error}")),
        }
    }
    Ok(HookOutput {
        result: HookResult {
            name: name.into(),
            command: expanded,
            status: if errors.is_empty() {
                "complete"
            } else {
                "failed"
            }
            .into(),
            exit_code,
            error: (!errors.is_empty()).then(|| errors.join("; ")),
            log_ids: logs.iter().map(|log| log.id.clone()).collect(),
            warnings,
            ..Default::default()
        },
        logs,
        stdout,
    })
}
