use crate::{
    collector,
    config::{BenchmarkConfig, BenchmarkMode, LoadedConfig},
    model::{BenchmarkResult, LogRef, Metric, RunMode},
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
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

/// isuscopeの全体期限は、adapterへ渡した期限よりこれだけ後ろに置く。adapterが自分の期限で
/// 止め、後始末と結果の出力を終えるための時間で、両方が同時に止めに行かないようにする。
pub const TIMEOUT_GRACE: Duration = Duration::from_secs(30);

pub struct BenchmarkExecution {
    pub result: BenchmarkResult,
    pub logs: Vec<LogRef>,
    pub metrics: Vec<Metric>,
}

#[derive(Default)]
struct Observation {
    operator_lines_dropped: usize,
    score: Option<i64>,
    passed: Option<bool>,
    pending: bool,
    messages: Vec<String>,
    initialize_started_at: Option<chrono::DateTime<Utc>>,
    initialize_finished_at: Option<chrono::DateTime<Utc>>,
    protocol_result_count: usize,
    /// 最後にstdoutかstderrへ行が出た時刻（運営向けで捨てた行も含む）。
    last_output: Option<Instant>,
}

pub async fn execute(
    config: &LoadedConfig,
    run_dir: &Path,
    shutdown: Shutdown,
    collect_metrics: bool,
    mode: RunMode,
) -> BenchmarkExecution {
    let started_at = Utc::now();
    let mut execution = match config.config.benchmark.mode {
        BenchmarkMode::Command => {
            match execute_command(config, run_dir, shutdown, collect_metrics, mode).await {
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
    // A command that could not be started is `not-started` and keeps no benchmark interval,
    // so it is never taken for a measurement (or as the base of the repeat check).
    if execution.result.mode != "not-started" {
        execution.result.started_at.get_or_insert(started_at);
        execution.result.finished_at.get_or_insert_with(Utc::now);
    }
    // 区間の境界はマイクロ秒で記録する（collectorへ渡す値、bucketの先頭と同じ精度）。
    let result = &mut execution.result;
    for at in [
        &mut result.started_at,
        &mut result.finished_at,
        &mut result.initialize_started_at,
        &mut result.initialize_finished_at,
    ] {
        *at = at.map(crate::model::to_micros);
    }
    execution
}

async fn execute_command(
    config: &LoadedConfig,
    run_dir: &Path,
    mut shutdown: Shutdown,
    collect_metrics: bool,
    mode: RunMode,
) -> Result<BenchmarkExecution> {
    let benchmark = &config.config.benchmark;
    let not_started = |error: anyhow::Error| BenchmarkExecution {
        result: BenchmarkResult {
            mode: "not-started".into(),
            command: benchmark.command.clone(),
            passed: Some(false),
            error: Some(format!("{error:#}")),
            ..Default::default()
        },
        logs: Vec::new(),
        metrics: Vec::new(),
    };
    let Some((program, args)) = benchmark.command.split_first() else {
        return Ok(not_started(anyhow::anyhow!(
            "benchmark.command must not be empty in command mode"
        )));
    };
    let score_pattern = match Regex::new(&benchmark.score_pattern)
        .context("benchmark.score_pattern is not a valid regular expression")
    {
        Ok(pattern) => pattern,
        Err(error) => return Ok(not_started(error)),
    };
    let operator_pattern = match benchmark
        .operator_line_pattern
        .as_deref()
        .map(Regex::new)
        .transpose()
        .context("benchmark.operator_line_pattern is not a valid regular expression")
    {
        Ok(pattern) => pattern,
        Err(error) => return Ok(not_started(error)),
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(config.benchmark_working_dir())
        .envs(&benchmark.env)
        .env("ISUSCOPE_BENCHMARK_PROTOCOL", "v1")
        .env("ISUSCOPE_PROJECT_ROOT", &config.project_root)
        .env("ISUSCOPE_RUN_DIR", run_dir)
        // survey-runだけ接続先を変えるなど、adapterが調査走行を区別できるようにする。
        .env("ISUSCOPE_RUN_MODE", mode.as_str())
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(seconds) = benchmark.timeout_seconds {
        command.env("ISUSCOPE_BENCHMARK_TIMEOUT_SECONDS", seconds.to_string());
    }
    process::configure_group(&mut command);
    let spawned = Instant::now();
    let mut child = match command.spawn().with_context(|| {
        format!(
            "cannot start benchmark command `{}`",
            benchmark.command.join(" ")
        )
    }) {
        Ok(child) => child,
        Err(error) => return Ok(not_started(error)),
    };
    let _registered = process::register_group(child.id());
    let stdout = child
        .stdout
        .take()
        .context("benchmark stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("benchmark stderr was not captured")?;
    let observation = Arc::new(Mutex::new(Observation::default()));
    let stdout_raw = run_dir.join("tmp/benchmark-stdout.log");
    let stderr_raw = run_dir.join("tmp/benchmark-stderr.log");
    let stdout_task = tokio::spawn(capture_lines(
        stdout,
        stdout_raw.clone(),
        false,
        observation.clone(),
        score_pattern.clone(),
        operator_pattern.clone(),
        benchmark.clone(),
    ));
    let stderr_task = tokio::spawn(capture_lines(
        stderr,
        stderr_raw.clone(),
        true,
        observation.clone(),
        score_pattern,
        operator_pattern,
        benchmark.clone(),
    ));
    // 全体の期限はadapterへ渡した値に猶予を足した時刻。出力の途絶えは0.5秒ごとに確かめる。
    let deadline = benchmark
        .timeout_seconds
        .map(|seconds| tokio::time::Instant::now() + Duration::from_secs(seconds) + TIMEOUT_GRACE);
    let idle_limit = benchmark.idle_timeout_seconds.map(Duration::from_secs);
    let (status, stopped_by) = loop {
        let stop = tokio::select! {
            status = child.wait() => {
                break (status.context("cannot wait for benchmark process")?, None);
            }
            _ = shutdown.cancelled() => "interrupted",
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => "timeout",
            _ = tokio::time::sleep(Duration::from_millis(500)), if idle_limit.is_some() => {
                let last = observation
                    .lock()
                    .ok()
                    .and_then(|observation| observation.last_output)
                    .unwrap_or(spawned);
                if idle_limit.is_some_and(|limit| last.elapsed() >= limit) {
                    "idle"
                } else {
                    continue;
                }
            }
        };
        let status = process::terminate_group(&mut child)
            .await
            .context("cannot terminate the benchmark process group")?;
        break (status, Some(stop));
    };
    let interrupted = stopped_by == Some("interrupted");
    // 中断後にpipeを握ったままの相手が残っていても、取り込みを待ち続けて止まらないようにする。
    let mut abandoned = Vec::new();
    let captures =
        process::finish_captures([stdout_task, stderr_task], process::CAPTURE_DEADLINE, None).await;
    for (capture, stream) in captures.into_iter().zip(["stdout", "stderr"]) {
        if finished_capture(capture, stream)? {
            abandoned.push(stream);
        }
    }

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

    let mut observation = Arc::try_unwrap(observation)
        .map_err(|_| anyhow::anyhow!("benchmark observation is still shared"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("benchmark observation lock is poisoned"))?;
    // 取り込みを打ち切ったことは、警告だけでなく保存するrunにも残す。
    for stream in abandoned {
        observation.messages.push(format!(
            "isuscope: benchmark {stream} stayed open for {}s after the process ended; the saved log may be incomplete",
            process::CAPTURE_DEADLINE.as_secs()
        ));
    }
    let duplicate_result = observation.protocol_result_count > 1;
    // 判定はadapterが`pass`で伝えたものだけを使う。終了コードとscoreから推測すると、判定の抽出を
    // 書き漏らしたadapterのFAILもPASSとして記録され、採否の比較に紛れ込む。
    let passed = if stopped_by.is_some() || !status.success() || duplicate_result {
        Some(false)
    } else {
        observation.passed
    };
    // 判定できない出力をadapterが`pending`で返したら、FAILにせず後から`rescore`で確定する。
    // 実出力でparserを確かめる前の初回だけに使う。
    let pending = passed.is_none() && observation.pending;
    Ok(BenchmarkExecution {
        result: BenchmarkResult {
            mode: "command".into(),
            command: benchmark.command.clone(),
            exit_code: status.code(),
            score: if pending { None } else { observation.score },
            passed,
            pending,
            interrupted,
            messages: observation.messages,
            started_at: None,
            finished_at: None,
            initialize_source: observation
                .initialize_finished_at
                .map(|_| "benchmark_output".into()),
            initialize_started_at: observation.initialize_started_at,
            initialize_finished_at: observation.initialize_finished_at,
            operator_lines_dropped: observation.operator_lines_dropped,
            stopped_by: stopped_by.map(str::to_owned),
            error: if interrupted {
                Some("interrupted by signal".into())
            } else if stopped_by == Some("timeout") {
                Some(format!(
                    "benchmark ran past [benchmark] timeout_seconds ({}s) and the {}s grace; isuscope stopped it",
                    benchmark.timeout_seconds.unwrap_or_default(),
                    TIMEOUT_GRACE.as_secs()
                ))
            } else if stopped_by == Some("idle") {
                Some(format!(
                    "benchmark printed nothing for [benchmark] idle_timeout_seconds ({}s); isuscope stopped it",
                    benchmark.idle_timeout_seconds.unwrap_or_default()
                ))
            } else if duplicate_result {
                Some(format!(
                    "benchmark emitted {} isuscope.result records; expected exactly one",
                    observation.protocol_result_count
                ))
            } else if !status.success() {
                Some(format!("benchmark command exited with {status}"))
            } else if passed.is_none() && !pending {
                Some(
                    "benchmark adapter reported no `pass` in its isuscope.result; fix .isuscope/benchmark.sh to emit the benchmark's own verdict"
                        .into(),
                )
            } else {
                None
            },
        },
        logs,
        metrics,
    })
}

/// 取り込みtaskの終わり方を読む。pipeの書き手が残って閉じなかった場合は打ち切られて
/// （taskは止めて、止まったことまで確かめてある）、そこまでに書けたlogで先へ進む。打ち切ったら
/// `true`。ベンチの側に残ったprocessは落とさない（ベンチが意図して残したものかもしれない）。
fn finished_capture(capture: process::Capture<Result<()>>, stream: &str) -> Result<bool> {
    match capture {
        process::Capture::Finished(joined) => {
            joined.with_context(|| format!("{stream} capture task failed"))??;
            Ok(false)
        }
        process::Capture::Abandoned => {
            eprintln!(
                "warning: benchmark {stream} stayed open for {}s after the process ended; \
                 the saved log may be incomplete",
                process::CAPTURE_DEADLINE.as_secs()
            );
            Ok(true)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn capture_lines<R>(
    reader: R,
    path: PathBuf,
    stderr: bool,
    observation: Arc<Mutex<Observation>>,
    score_pattern: Regex,
    operator_pattern: Option<Regex>,
    config: BenchmarkConfig,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    // Keep the bytes as written: a single invalid UTF-8 sequence (for example from two
    // streams interleaved by ssh) must not end the capture and lose the rest of the output.
    let mut reader = BufReader::new(reader);
    let mut file = tokio::fs::File::create(path).await?;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer).await? == 0 {
            break;
        }
        if buffer.last() != Some(&b'\n') {
            buffer.push(b'\n');
        }
        let raw = &buffer[..buffer.len() - 1];
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let line = String::from_utf8_lossy(raw);
        let line = line.as_ref();
        if let Ok(mut observation) = observation.lock() {
            observation.last_output = Some(Instant::now());
        }
        // Organizer-only lines are part of the rules. Drop them before anything is written,
        // so no saved log, metric or view can be read back from them.
        if operator_pattern
            .as_ref()
            .is_some_and(|pattern| pattern.is_match(line))
        {
            if let Ok(mut observation) = observation.lock() {
                observation.operator_lines_dropped += 1;
            }
            continue;
        }
        file.write_all(&buffer).await?;
        if config.stream_output {
            if stderr {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
        observe_line(line, stderr, &score_pattern, &config, &observation);
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
            if value.get("pending").and_then(Value::as_bool) == Some(true) {
                observation.pending = true;
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

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

pub fn compress_log(source: &Path, destination: &Path) -> Result<()> {
    // 大きいログ（生のaccess log、状態ログ）はcollectorがnode上でzstdにしてから送るので、
    // 既にzstdならそのまま置く。
    let mut magic = [0u8; 4];
    let is_zstd = {
        use std::io::Read;
        fs::File::open(source)?.read(&mut magic)? == 4 && magic == ZSTD_MAGIC
    };
    if is_zstd {
        fs::rename(source, destination)?;
        return Ok(());
    }
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
            operator_line_pattern: None,
            parsers: Vec::new(),
            sample_output: None,
            prepare_command: None,
            identity_command: None,
            cleanup_command: None,
            timeout_seconds: None,
            idle_timeout_seconds: None,
            hook_timeout_seconds: 120,
            rescore_command: Vec::new(),
        }
    }

    #[test]
    fn compress_log_keeps_output_that_is_already_zstd() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.log");
        let packed = dir.path().join("packed.log");
        fs::write(&plain, "a\nb\n").unwrap();
        fs::write(&packed, zstd::encode_all(&b"c\n"[..], 1).unwrap()).unwrap();
        for (source, expected) in [(&plain, "a\nb\n"), (&packed, "c\n")] {
            let destination = source.with_extension("zst");
            compress_log(source, &destination).unwrap();
            assert!(!source.exists());
            let decoded = zstd::decode_all(fs::File::open(&destination).unwrap()).unwrap();
            assert_eq!(String::from_utf8(decoded).unwrap(), expected);
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
