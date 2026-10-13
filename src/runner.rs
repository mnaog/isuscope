use crate::{
    agent_context, benchmark,
    collector::{self, CollectorOutput},
    config::{CollectorPhase, LoadedConfig, Transport},
    enrichment::{self, EnrichmentOutput},
    git_snapshot,
    hooks::{self, HookContext},
    model::{
        AnalysisStatus, BenchmarkMessageKind, BenchmarkResult, RunManifest, RunMode, RunState,
        SourceSnapshot, StageTiming, ToolingSnapshot,
    },
    repeat,
    shutdown::Shutdown,
    storage::Store,
    tooling,
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::fs;
use uuid::Uuid;

pub struct RunOutcome {
    pub id: String,
    pub passed: bool,
    pub state: RunState,
    pub score: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct RunAnnotations {
    pub hypothesis: String,
    pub note: Option<String>,
    pub tags: Vec<String>,
    /// `--repeat`: why the same configuration as the last benchmark is measured again.
    pub repeat: Option<String>,
}

pub async fn execute(
    config: LoadedConfig,
    mode: RunMode,
    shutdown: Shutdown,
    mut annotations: RunAnnotations,
) -> Result<RunOutcome> {
    if mode == RunMode::ScoreRun {
        bail!(
            "score-run is no longer supported; remove observation settings and use `isuscope run`"
        );
    }
    if annotations.hypothesis.trim().is_empty() {
        bail!("hypothesis must not be empty");
    }
    let mut store = Store::open(&config.data_dir)?;
    // 実行中runの確認から`run.json`を書くまでの間に、別の`run`が同じ確認を通らないよう、
    // 入口で握って終わるまで持つ。
    let Some(_gate) = crate::lock::RunGate::try_acquire(&store.run_gate_path())? else {
        bail!(
            "another isuscope run is still in progress in this data directory; wait for it to finish"
        );
    };
    let recovery = store.recover_incomplete()?;
    if !recovery.recovered.is_empty() {
        for id in &recovery.recovered {
            println!("recovered  {} (aborted)", short_id(id));
        }
        collector::cleanup_abandoned(&config, &recovery.recovered).await;
    }
    // 同じdata directoryで2つのベンチを重ねない。`[lock]`を設定していなくても、
    // 実行中のrunの印でここは守る。
    if let Some(active) = recovery.active.first() {
        bail!(
            "run {} is still in progress in another isuscope process; wait for it to finish",
            short_id(active)
        );
    }
    let pending = store.pending_analyses()?;
    if let Some(run) = pending.first() {
        bail!(
            "run {} is still awaiting analysis for hypothesis `{}`; run `isuscope analyze {}` before starting another benchmark",
            short_id(&run.id),
            run.hypothesis,
            short_id(&run.id),
        );
    }
    check_node_disks(&config).await?;
    let agent_context = agent_context::resolve(&config)?;
    let id = Uuid::now_v7().to_string();
    // run.jsonを書く前に印を握る。握る前に書くと、別processの回収処理に中断runと誤認される。
    let _marker = crate::lock::RunMarker::try_hold(&store.run_marker_path(&id))?
        .context("cannot hold the marker of a new run")?;
    let staging = store.staging_dir(&id);
    fs::create_dir_all(staging.join("source"))?;
    fs::create_dir_all(staging.join("logs"))?;
    fs::create_dir_all(staging.join("tmp"))?;
    if let Some(context) = &agent_context {
        context.write_snapshot(&staging)?;
    }

    let mut source_excludes = config.config.source.exclude.clone();
    if let Some(history_dir) = config.config.context.history_dir() {
        source_excludes.push(history_dir.to_path_buf());
    }
    let source = git_snapshot::capture(
        &config.source_repo(),
        &staging.join("source"),
        &source_excludes,
    )
    .unwrap_or_else(|error| SourceSnapshot {
        repository: config.source_repo().display().to_string(),
        dirty: true,
        error: Some(format!("{error:#}")),
        ..Default::default()
    });
    let tooling = tooling::capture(&config, &staging.join("tooling")).unwrap_or_else(|error| {
        ToolingSnapshot {
            isuscope_version: env!("CARGO_PKG_VERSION").into(),
            error: Some(format!("{error:#}")),
            ..Default::default()
        }
    });
    let started_at = Utc::now();
    annotations.tags.sort();
    annotations.tags.dedup();
    let mut manifest = RunManifest {
        schema_version: 6,
        id: id.clone(),
        mode,
        state: RunState::Running,
        started_at,
        finished_at: None,
        hypothesis: annotations.hypothesis,
        analysis_status: AnalysisStatus::Pending,
        analyses: Vec::new(),
        note: annotations.note.filter(|note| !note.trim().is_empty()),
        tags: annotations.tags,
        source,
        tooling,
        agent_context: agent_context.map(|context| context.metadata),
        benchmark: BenchmarkResult::default(),
        collectors: Vec::new(),
        enrichments: Vec::new(),
        logs: Vec::new(),
        metric_count: 0,
        fingerprint_count: 0,
        transition_count: 0,
        structured_snapshot: None,
        stages: Vec::new(),
        hooks: Vec::new(),
        identity: None,
        repeat: None,
    };
    store.begin(&manifest)?;

    print_header(&manifest);
    let mut metrics = Vec::new();
    let mut fingerprints = Vec::new();
    let mut transitions = Vec::new();
    let mut after_outputs = None;

    let repeat_reason = annotations
        .repeat
        .take()
        .map(|reason| reason.trim().to_owned())
        .filter(|reason| !reason.is_empty());
    let hook_context = HookContext {
        config: &config,
        run_id: &id,
        run_dir: &staging,
        mode,
        env: repeat_reason
            .iter()
            .map(|reason| ("ISUSCOPE_REPEAT_REASON", reason.clone()))
            .collect(),
    };
    let benchmark_config = &config.config.benchmark;
    if let Some(command) = &benchmark_config.identity_command {
        println!("→ identity");
        let output = hooks::run(&hook_context, "identity", command).await;
        if output.result.status == "complete" {
            manifest.identity = Some(repeat::identity(&output.stdout));
        }
        absorb_hook(output, &mut manifest);
    }
    let mut prepare_error = None;
    if let Some(command) = &benchmark_config.prepare_command {
        println!("→ prepare");
        let output = timed(
            "prepare",
            &mut manifest.stages,
            hooks::run(&hook_context, "prepare", command),
        )
        .await;
        if output.result.status != "complete" {
            prepare_error = Some(format!(
                "prepare_command failed: {}",
                output.result.error.as_deref().unwrap_or("unknown error")
            ));
        }
        absorb_hook(output, &mut manifest);
    }

    let (before, required_failed, unreachable) = if prepare_error.is_some() {
        (Vec::new(), false, Vec::new())
    } else {
        println!("→ collectors: before");
        let before = timed(
            "collectors_before",
            &mut manifest.stages,
            collector::run_phase(
                &config,
                mode,
                CollectorPhase::Before,
                &id,
                &staging,
                Some(shutdown.clone()),
            ),
        )
        .await;
        let required_failed = before.iter().any(|output| {
            output.result.status == "failed"
                && config.config.collectors.iter().any(|collector| {
                    matches!(collector.phase, CollectorPhase::Before)
                        && collector.name == output.result.name
                        && collector.required
                })
        });
        let unreachable = unreachable_ssh_nodes(&config, &before);
        (before, required_failed, unreachable)
    };
    absorb(
        before,
        &mut manifest,
        &mut metrics,
        &mut fingerprints,
        &mut transitions,
    );
    if prepare_error.is_none() {
        manifest.repeat = check_repeat(&store, &manifest, &fingerprints, repeat_reason.clone())?;
    }
    let refused_as_repeat = manifest
        .repeat
        .as_ref()
        .is_some_and(|check| check.changed.is_empty() && check.reason.is_none());

    if let Some(error) = prepare_error {
        eprintln!("! {error}");
        manifest.benchmark = BenchmarkResult {
            mode: "not-started".into(),
            passed: Some(false),
            error: Some(error),
            ..Default::default()
        };
        // before collectorを動かしていないので、後片付けするものも無い。
        after_outputs = Some(Vec::new());
    } else if shutdown.is_cancelled() {
        manifest.benchmark = interrupted_benchmark();
    } else if required_failed {
        manifest.benchmark = BenchmarkResult {
            mode: "not-started".into(),
            passed: Some(false),
            error: Some("a required before collector failed".into()),
            ..Default::default()
        };
    } else if !unreachable.is_empty() {
        let error = format!(
            "SSH failed for every before collector on {}; fix SSH (host keys, identity, reachability) before benchmarking",
            unreachable.join(", ")
        );
        eprintln!("! {error}");
        manifest.benchmark = BenchmarkResult {
            mode: "not-started".into(),
            passed: Some(false),
            error: Some(error),
            ..Default::default()
        };
    } else if refused_as_repeat {
        let base = manifest
            .repeat
            .as_ref()
            .map(|check| short_id(&check.base_run_id).to_owned())
            .unwrap_or_default();
        let error = format!(
            "nothing isuscope can see changed since run {base} (deployment, node fingerprints, observation settings); deploy the change, or pass --repeat \"<reason>\" to measure the same configuration again"
        );
        eprintln!("! {error}");
        manifest.benchmark = BenchmarkResult {
            mode: "not-started".into(),
            passed: Some(false),
            error: Some(error),
            ..Default::default()
        };
    } else {
        println!("→ collectors: during");
        let (running, startup_failures) = timed(
            "collectors_during_start",
            &mut manifest.stages,
            collector::start_during(&config, mode, &id, &staging),
        )
        .await;
        absorb(
            startup_failures,
            &mut manifest,
            &mut metrics,
            &mut fingerprints,
            &mut transitions,
        );

        println!("→ benchmark");
        let execution = timed(
            "benchmark",
            &mut manifest.stages,
            benchmark::execute(&config, &staging, shutdown.clone(), true, mode),
        )
        .await;
        manifest.benchmark = execution.result;
        manifest.logs.extend(execution.logs);
        metrics.extend(execution.metrics);
        if let Some(stopped_by) = manifest.benchmark.stopped_by.clone() {
            eprintln!(
                "! benchmark stopped by isuscope ({stopped_by}): {}",
                manifest.benchmark.error.as_deref().unwrap_or_default()
            );
            if let Some(command) = &benchmark_config.cleanup_command {
                println!("→ cleanup");
                let output = timed(
                    "cleanup",
                    &mut manifest.stages,
                    hooks::run(&hook_context, "cleanup", command),
                )
                .await;
                absorb_hook(output, &mut manifest);
            }
        }
        // ベンチの出力に境界が無ければ、collector（access logの最後のinitialize要求など）から取る。
        // during collectorの解析とafter collectorの区間はこの境界で決まるので、その前に済ませる。
        if manifest.benchmark.started_at.is_some()
            && manifest.benchmark.initialize_finished_at.is_none()
            && collector::selected(&config, mode, CollectorPhase::Boundary)
                .next()
                .is_some()
        {
            store.checkpoint(&manifest)?;
            println!("→ collectors: boundary");
            let boundary = timed(
                "collectors_boundary",
                &mut manifest.stages,
                collector::run_phase(&config, mode, CollectorPhase::Boundary, &id, &staging, None),
            )
            .await;
            apply_boundary(&mut manifest, &staging, &boundary);
            absorb(
                boundary,
                &mut manifest,
                &mut metrics,
                &mut fingerprints,
                &mut transitions,
            );
        }
        store.checkpoint(&manifest)?;

        // benchmark parserはベンチの出力だけを読むので、during collectorの停止とafter collectorを
        // 待たずに並行して動かす。表示と保存の順は、並行にする前と同じにする。
        let mut parser_stages = Vec::new();
        let mut collector_stages = Vec::new();
        let (enriched, (during, after)) = tokio::join!(
            timed(
                "benchmark_parsers",
                &mut parser_stages,
                enrichment::run_all(&config, &id, &staging, &id),
            ),
            async {
                let during = timed(
                    "collectors_during_stop",
                    &mut collector_stages,
                    collector::stop_during(running, &staging),
                )
                .await;
                let after = timed(
                    "collectors_after",
                    &mut collector_stages,
                    collector::run_phase(&config, mode, CollectorPhase::After, &id, &staging, None),
                )
                .await;
                (during, after)
            },
        );
        manifest.stages.extend(collector_stages);
        manifest.stages.extend(parser_stages);
        absorb(
            during,
            &mut manifest,
            &mut metrics,
            &mut fingerprints,
            &mut transitions,
        );

        println!("→ benchmark parsers");
        absorb_enrichments(enriched, &mut manifest, &mut metrics);
        after_outputs = Some(after);
    }

    println!("→ collectors: after");
    let after = match after_outputs {
        Some(after) => after,
        None => {
            timed(
                "collectors_after",
                &mut manifest.stages,
                collector::run_phase(&config, mode, CollectorPhase::After, &id, &staging, None),
            )
            .await
        }
    };
    absorb(
        after,
        &mut manifest,
        &mut metrics,
        &mut fingerprints,
        &mut transitions,
    );
    // nodeへの操作はここまで。保存の間に次のdeployを始められるよう、共通lockを先に手放す。
    crate::lock::release_run_lock();

    if let (Some(start), Some(end)) = (
        manifest.benchmark.initialize_started_at,
        manifest.benchmark.initialize_finished_at,
    ) {
        metrics.push(crate::model::Metric {
            name: "benchmark.initialize_duration".into(),
            value: (end - start).num_microseconds().unwrap_or_default() as f64 / 1_000.0,
            unit: "ms".into(),
            timestamp: None,
            labels: Default::default(),
        });
    }

    manifest.settle_state();
    manifest.finished_at = Some(Utc::now());
    manifest.analysis_status = if manifest.benchmark.passed == Some(true) {
        AnalysisStatus::Pending
    } else {
        AnalysisStatus::NotRequired
    };
    manifest.metric_count = metrics.len();
    manifest.fingerprint_count = fingerprints.len();
    manifest.transition_count = transitions.len();
    store.finish(&manifest, &metrics, &fingerprints, &transitions)?;

    println!();
    println!("run       {}", short_id(&id));
    println!(
        "result    {}",
        match manifest.benchmark.passed {
            Some(true) => "PASS",
            Some(false) => "FAIL",
            None => "UNKNOWN",
        }
    );
    println!(
        "score     {}",
        manifest
            .benchmark
            .score
            .map(|score| score.to_string())
            .unwrap_or_else(|| "-".into())
    );
    println!("state     {}", manifest.state.as_str());
    if !manifest.stages.is_empty() {
        println!("timing    {}", describe_stages(&manifest.stages));
    }
    for line in describe_configuration(&manifest) {
        println!("{line}");
    }
    for reason in manifest.failure_reasons().iter().take(3) {
        println!("failure   {reason}");
    }
    let mut error_categories = Vec::<Option<&str>>::new();
    for message in manifest.benchmark_messages(BenchmarkMessageKind::Error) {
        let category = message.category.as_deref();
        if !error_categories.contains(&category) && error_categories.len() < 3 {
            error_categories.push(category);
            println!("error     {}", message.text);
        }
    }
    if manifest.benchmark.operator_lines_dropped > 0 {
        println!(
            "operator  {} organizer-only lines dropped",
            manifest.benchmark.operator_lines_dropped
        );
    }
    println!("analysis  {}", manifest.analysis_status.as_str());
    if manifest.analysis_status == AnalysisStatus::Pending {
        println!();
        println!(
            "next      isuscope analyze {} <supported|rejected|inconclusive> --analysis <text>",
            short_id(&id)
        );
    }

    Ok(RunOutcome {
        id,
        passed: manifest.benchmark.passed == Some(true),
        state: manifest.state,
        score: manifest.benchmark.score,
    })
}

/// Runs one stage of the run and records when it started and how long it took.
async fn timed<T>(
    name: &str,
    stages: &mut Vec<StageTiming>,
    stage: impl std::future::Future<Output = T>,
) -> T {
    let started_at = Utc::now();
    let started = std::time::Instant::now();
    let value = stage.await;
    stages.push(StageTiming {
        name: name.into(),
        started_at,
        duration_ms: started.elapsed().as_millis() as u64,
    });
    value
}

/// What was measured and how it relates to the last benchmark: the deployment identity, the
/// differences (or the reason for measuring the same thing again), why isuscope stopped the
/// benchmark, and where the initialize boundary came from.
fn describe_configuration(manifest: &RunManifest) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(identity) = &manifest.identity {
        let mut fields = identity
            .values
            .iter()
            .take(6)
            .map(|(key, value)| format!("{key}={}", value.chars().take(16).collect::<String>()))
            .collect::<Vec<_>>();
        if fields.is_empty() {
            fields.push(identity.sha256.chars().take(12).collect());
        }
        lines.push(format!("identity  {}", fields.join(" ")));
    }
    if let Some(check) = &manifest.repeat {
        let base = short_id(&check.base_run_id);
        match &check.reason {
            Some(reason) => lines.push(format!("repeat    of {base}: {reason}")),
            None if check.changed.is_empty() => {
                lines.push(format!("repeat    refused: same as {base}"))
            }
            None => {
                let mut changed = check.changed.iter().take(5).cloned().collect::<Vec<_>>();
                if check.changed.len() > 5 {
                    changed.push(format!("+{} more", check.changed.len() - 5));
                }
                lines.push(format!("changed   {} (since {base})", changed.join(", ")));
            }
        }
    }
    if let Some(stopped_by) = &manifest.benchmark.stopped_by {
        lines.push(format!("stopped   {stopped_by}"));
    }
    if manifest.benchmark.mode != "not-started" && manifest.benchmark.started_at.is_some() {
        lines.push(format!(
            "initialize {}",
            manifest
                .benchmark
                .initialize_source
                .as_deref()
                .unwrap_or("unknown (summaries include initialize)")
        ));
    }
    lines
}

fn describe_stages(stages: &[StageTiming]) -> String {
    stages
        .iter()
        .map(|stage| format!("{} {:.1}s", stage.name, stage.duration_ms as f64 / 1_000.0))
        .collect::<Vec<_>>()
        .join(", ")
}

fn absorb_hook(output: hooks::HookOutput, manifest: &mut RunManifest) {
    let result = &output.result;
    let seconds = result.duration_ms.unwrap_or_default() as f64 / 1_000.0;
    if result.status == "complete" {
        println!("✓ {} ({seconds:.1}s)", result.name);
    } else {
        println!(
            "! {} ({seconds:.1}s): {}",
            result.name,
            result.error.as_deref().unwrap_or("failed")
        );
    }
    manifest.logs.extend(output.logs);
    manifest.hooks.push(output.result);
}

/// Compares this run with the last one whose benchmark started. `None` when there is none.
fn check_repeat(
    store: &Store,
    manifest: &RunManifest,
    fingerprints: &[crate::model::Fingerprint],
    reason: Option<String>,
) -> Result<Option<crate::model::RepeatCheck>> {
    let Some(base) = store.last_started_run(&manifest.id)? else {
        return Ok(None);
    };
    let base_fingerprints = store.fingerprint_index(&base.id)?.unwrap_or_default();
    let mut current_fingerprints = std::collections::BTreeMap::new();
    for fingerprint in fingerprints {
        current_fingerprints.insert(
            crate::storage::fingerprint_key(&fingerprint.name, &fingerprint.labels)?,
            fingerprint.value.clone(),
        );
    }
    Ok(Some(repeat::compare(
        &base,
        &base_fingerprints,
        manifest,
        &current_fingerprints,
        reason,
    )))
}

/// Takes the initialize boundary that a boundary collector reported. Several nodes (more
/// than one nginx) may answer; the latest end within the benchmark wins, since the load
/// starts after the last initialize.
fn apply_boundary(
    manifest: &mut RunManifest,
    run_dir: &std::path::Path,
    outputs: &[collector::CollectorOutput],
) {
    let (Some(begin), Some(end)) = (
        manifest.benchmark.started_at,
        manifest.benchmark.finished_at,
    ) else {
        return;
    };
    let epoch = |value: Option<&serde_json::Value>| {
        let seconds = value?.as_f64()?;
        chrono::DateTime::from_timestamp_micros((seconds * 1_000_000.0).round() as i64)
    };
    let mut best: Option<(chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>)> = None;
    for output in outputs {
        for event in collector::read_events(run_dir, output) {
            if event.get("name").and_then(serde_json::Value::as_str) != Some("initialize") {
                continue;
            }
            let Some(finished) = epoch(event.get("finished_at")) else {
                continue;
            };
            if finished < begin || finished >= end {
                continue;
            }
            if best.is_none_or(|(current, _)| finished > current) {
                best = Some((finished, epoch(event.get("started_at"))));
            }
        }
    }
    if let Some((finished, started)) = best {
        let started = started
            .filter(|started| *started <= finished)
            .unwrap_or(begin);
        manifest.benchmark.initialize_started_at =
            Some(crate::model::to_micros(started.max(begin)));
        manifest.benchmark.initialize_finished_at = Some(crate::model::to_micros(finished));
        manifest.benchmark.initialize_source = Some("access_log".into());
        println!(
            "initialize boundary from the access log: load starts at {}",
            finished.to_rfc3339()
        );
    }
}

fn absorb_enrichments(
    outputs: Vec<EnrichmentOutput>,
    manifest: &mut RunManifest,
    metrics: &mut Vec<crate::model::Metric>,
) {
    for output in outputs {
        let symbol = if output.result.status == "complete" {
            "✓"
        } else {
            "!"
        };
        println!(
            "{symbol} {} ({}, {} metrics)",
            output.result.name,
            output.result.status,
            output.metrics.len()
        );
        manifest.enrichments.push(output.result);
        manifest.logs.extend(output.logs);
        metrics.extend(output.metrics);
    }
}

fn interrupted_benchmark() -> BenchmarkResult {
    BenchmarkResult {
        mode: "not-started".into(),
        passed: Some(false),
        interrupted: true,
        error: Some("interrupted by signal".into()),
        ..Default::default()
    }
}

fn absorb(
    outputs: Vec<CollectorOutput>,
    manifest: &mut RunManifest,
    metrics: &mut Vec<crate::model::Metric>,
    fingerprints: &mut Vec<crate::model::Fingerprint>,
    transitions: &mut Vec<crate::model::Transition>,
) {
    for output in outputs {
        let status_symbol = match output.result.status.as_str() {
            "complete" => "✓",
            "unavailable" => "-",
            _ => "!",
        };
        let node = output.result.node.as_deref().unwrap_or("local");
        println!(
            "{status_symbol} {} ({node}, {})",
            output.result.name, output.result.status
        );
        manifest.collectors.push(output.result);
        manifest.logs.extend(output.logs);
        metrics.extend(output.metrics);
        fingerprints.extend(output.fingerprints);
        transitions.extend(output.transitions);
    }
}

fn print_header(manifest: &RunManifest) {
    println!("run       {}", short_id(&manifest.id));
    println!("mode      {}", manifest.mode.as_str());
    println!("hypothesis {}", manifest.hypothesis);
    let commit = manifest
        .source
        .commit_hash
        .as_deref()
        .map(|value| &value[..value.len().min(12)])
        .unwrap_or("no-git");
    println!(
        "source    {}{}",
        commit,
        if manifest.source.dirty {
            " (dirty)"
        } else {
            ""
        }
    );
    if let Some(context) = &manifest.agent_context {
        println!(
            "context   {} {}#{}",
            context.agent,
            context.history_path,
            short_id(&context.input_id)
        );
    }
    println!();
}

/// Warns about nodes low on disk and refuses to benchmark when one is below the minimum.
/// Unreachable nodes are left to the before collectors, which report SSH failures.
async fn check_node_disks(config: &LoadedConfig) -> Result<()> {
    use crate::node_disk::{self, DiskLevel};
    let disk = &config.config.disk;
    let mut too_low = Vec::new();
    for node in node_disk::measure(config).await {
        match node.tightest(disk) {
            Some((mount, DiskLevel::Low)) => {
                eprintln!("! disk {}: {}", node.node, node_disk::describe(mount));
            }
            Some((mount, DiskLevel::TooLow)) => {
                too_low.push(format!("{}: {}", node.node, node_disk::describe(mount)));
            }
            _ => {}
        }
    }
    if !too_low.is_empty() {
        bail!(
            "nodes are below the {} MiB free disk minimum ({}); free space before benchmarking, or lower [disk] node_min_free_mb",
            disk.node_min_free_mb,
            too_low.join(", ")
        );
    }
    Ok(())
}

pub fn short_id(id: &str) -> &str {
    let start = id.len().saturating_sub(8);
    &id[start..]
}

/// SSH exits with 255 when the connection itself fails. A node whose every SSH before
/// collector failed that way would produce a benchmark without any of its observations.
fn unreachable_ssh_nodes(
    config: &LoadedConfig,
    outputs: &[collector::CollectorOutput],
) -> Vec<String> {
    let ssh_before = config
        .config
        .collectors
        .iter()
        .filter(|collector| {
            matches!(collector.phase, CollectorPhase::Before)
                && matches!(collector.transport, Transport::Ssh)
        })
        .map(|collector| collector.name.as_str())
        .collect::<Vec<_>>();
    let mut by_node = std::collections::BTreeMap::<&str, bool>::new();
    for output in outputs {
        let result = &output.result;
        let Some(node) = result.node.as_deref() else {
            continue;
        };
        if !ssh_before.contains(&result.name.as_str()) {
            continue;
        }
        let transport_failed = result.status == "failed" && result.exit_code == Some(255);
        let entry = by_node.entry(node).or_insert(true);
        *entry &= transport_failed;
    }
    by_node
        .into_iter()
        .filter(|(_, failed)| *failed)
        .map(|(node, _)| node.to_owned())
        .collect()
}
