use crate::{
    benchmark,
    collector::{self, CollectorOutput},
    config::{CollectorPhase, LoadedConfig},
    enrichment::{self, EnrichmentOutput},
    git_snapshot,
    model::{
        AnalysisStatus, BenchmarkResult, RunManifest, RunMode, RunState, SourceSnapshot,
        ToolingSnapshot,
    },
    shutdown::Shutdown,
    storage::Store,
    tooling,
};
use anyhow::{Result, bail};
use chrono::Utc;
use std::{fs, path::Path};
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
    if annotations.hypothesis.trim().is_empty() {
        bail!("hypothesis must not be empty");
    }
    let mut store = Store::open(&config.data_dir)?;
    let recovered = store.recover_incomplete()?;
    if !recovered.is_empty() {
        for id in &recovered {
            println!("recovered  {} (aborted)", short_id(id));
        }
        collector::cleanup_abandoned(&config, &recovered).await;
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
    let id = Uuid::now_v7().to_string();
    let staging = store.staging_dir(&id);
    fs::create_dir_all(staging.join("source"))?;
    fs::create_dir_all(staging.join("logs"))?;
    fs::create_dir_all(staging.join("tmp"))?;

    let source = git_snapshot::capture(
        &config.source_repo(),
        &staging.join("source"),
        &config.config.source.exclude,
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
        schema_version: 5,
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
        benchmark: BenchmarkResult::default(),
        collectors: Vec::new(),
        enrichments: Vec::new(),
        logs: Vec::new(),
        metric_count: 0,
        fingerprint_count: 0,
        transition_count: 0,
    };
    store.begin(&manifest)?;

    print_header(&manifest, &config);
    let mut metrics = Vec::new();
    let mut fingerprints = Vec::new();
    let mut transitions = Vec::new();

    let score_only = mode == RunMode::ScoreRun;
    if score_only {
        println!("→ collectors: skipped (score-run)");
    } else {
        println!("→ collectors: before");
    }
    let before = if score_only {
        Vec::new()
    } else {
        collector::run_phase(
            &config,
            mode,
            CollectorPhase::Before,
            &id,
            &staging,
            Some(shutdown.clone()),
        )
        .await
    };
    let required_failed = before.iter().any(|output| {
        output.result.status == "failed"
            && config
                .config
                .collectors
                .iter()
                .any(|collector| collector.name == output.result.name && collector.required)
    });
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
    } else {
        if !score_only {
            println!("→ collectors: during");
        }
        let (running, startup_failures) = if score_only {
            (Vec::new(), Vec::new())
        } else {
            collector::start_during(&config, mode, &id, &staging).await
        };
        absorb(
            startup_failures,
            &mut manifest,
            &mut metrics,
            &mut fingerprints,
            &mut transitions,
        );

        println!("→ benchmark");
        let execution = benchmark::execute(&config, &staging, shutdown.clone(), !score_only).await;
        manifest.benchmark = execution.result;
        manifest.logs.extend(execution.logs);
        store.checkpoint(&manifest)?;
        if !score_only {
            metrics.extend(execution.metrics);
        }

        let during = collector::stop_during(running, &staging).await;
        absorb(
            during,
            &mut manifest,
            &mut metrics,
            &mut fingerprints,
            &mut transitions,
        );

        if !score_only {
            println!("→ benchmark parsers");
            let enriched = enrichment::run_all(&config, &id, &staging).await;
            absorb_enrichments(enriched, &mut manifest, &mut metrics);
        }
    }

    if !score_only {
        println!("→ collectors: after");
    }
    let after = if score_only {
        Vec::new()
    } else {
        collector::run_phase(&config, mode, CollectorPhase::After, &id, &staging, None).await
    };
    absorb(
        after,
        &mut manifest,
        &mut metrics,
        &mut fingerprints,
        &mut transitions,
    );

    if !score_only
        && let (Some(start), Some(end)) = (
            manifest.benchmark.initialize_started_at,
            manifest.benchmark.initialize_finished_at,
        )
    {
        metrics.push(crate::model::Metric {
            name: "benchmark.initialize_duration".into(),
            value: (end - start).num_microseconds().unwrap_or_default() as f64 / 1_000.0,
            unit: "ms".into(),
            timestamp: None,
            labels: Default::default(),
        });
    }

    let collector_degraded = manifest
        .collectors
        .iter()
        .any(|collector| collector.status == "failed");
    let enrichment_degraded = manifest
        .enrichments
        .iter()
        .any(|enrichment| enrichment.status == "failed");
    manifest.state = match manifest.benchmark.passed {
        _ if manifest.benchmark.interrupted => RunState::Aborted,
        Some(true) if collector_degraded || enrichment_degraded => RunState::Degraded,
        Some(true) => RunState::Complete,
        _ => RunState::Failed,
    };
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
    println!("analysis  {}", manifest.analysis_status.as_str());
    println!("saved     {}", final_dir.display());
    if manifest.analysis_status == AnalysisStatus::Pending {
        println!();
        println!(
            "next      isuscope analyze {} --verdict <supported|rejected|inconclusive> --analysis <text>",
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
    println!("data      {}", config.data_dir.display());
    println!();
}

pub fn short_id(id: &str) -> &str {
    let start = id.len().saturating_sub(8);
    &id[start..]
}

pub fn run_path(data_dir: &Path, id: &str) -> std::path::PathBuf {
    data_dir.join("runs").join(id)
}
