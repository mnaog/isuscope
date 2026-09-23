use crate::{
    agent_context, benchmark,
    collector::{self, CollectorOutput},
    config::{CollectorPhase, LoadedConfig, Transport},
    enrichment::{self, EnrichmentOutput},
    git_snapshot,
    model::{
        AnalysisStatus, BenchmarkMessageKind, BenchmarkResult, RunManifest, RunMode, RunState,
        SourceSnapshot, ToolingSnapshot,
    },
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
    };
    store.begin(&manifest)?;

    print_header(&manifest, &config);
    let mut metrics = Vec::new();
    let mut fingerprints = Vec::new();
    let mut transitions = Vec::new();

    println!("→ collectors: before");
    let before = collector::run_phase(
        &config,
        mode,
        CollectorPhase::Before,
        &id,
        &staging,
        Some(shutdown.clone()),
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
    absorb(
        before,
        &mut manifest,
        &mut metrics,
        &mut fingerprints,
        &mut transitions,
    );

    if shutdown.is_cancelled() {
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
    } else {
        println!("→ collectors: during");
        let (running, startup_failures) =
            collector::start_during(&config, mode, &id, &staging).await;
        absorb(
            startup_failures,
            &mut manifest,
            &mut metrics,
            &mut fingerprints,
            &mut transitions,
        );

        println!("→ benchmark");
        let execution = benchmark::execute(&config, &staging, shutdown.clone(), true, mode).await;
        manifest.benchmark = execution.result;
        manifest.logs.extend(execution.logs);
        store.checkpoint(&manifest)?;
        metrics.extend(execution.metrics);

        let during = collector::stop_during(running, &staging).await;
        absorb(
            during,
            &mut manifest,
            &mut metrics,
            &mut fingerprints,
            &mut transitions,
        );

        println!("→ benchmark parsers");
        let enriched = enrichment::run_all(&config, &id, &staging, &id).await;
        absorb_enrichments(enriched, &mut manifest, &mut metrics);
    }

    println!("→ collectors: after");
    let after =
        collector::run_phase(&config, mode, CollectorPhase::After, &id, &staging, None).await;
    absorb(
        after,
        &mut manifest,
        &mut metrics,
        &mut fingerprints,
        &mut transitions,
    );

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
    let final_dir = store.finish(&manifest, &metrics, &fingerprints, &transitions)?;

    println!();
    println!("run       {}", short_id(&id));
    println!(
        "result    {}",
        if manifest.benchmark.passed == Some(true) {
            "PASS"
        } else {
            "FAIL"
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
    println!("saved     {}", final_dir.display());
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

fn print_header(manifest: &RunManifest, config: &LoadedConfig) {
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
    println!("data      {}", config.data_dir.display());
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
