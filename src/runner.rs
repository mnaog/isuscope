use crate::{
    benchmark,
    collector::{self, CollectorOutput},
    config::{CollectorPhase, LoadedConfig},
    git_snapshot,
    model::{BenchmarkResult, RunManifest, RunMode, RunState, SourceSnapshot, ToolingSnapshot},
    shutdown::Shutdown,
    storage::Store,
    tooling,
};
use anyhow::Result;
use chrono::Utc;
use std::{fs, path::Path};
use uuid::Uuid;

pub struct RunOutcome {
    pub id: String,
    pub passed: bool,
    pub state: RunState,
    pub score: Option<i64>,
}

pub async fn execute(
    config: LoadedConfig,
    mode: RunMode,
    shutdown: Shutdown,
) -> Result<RunOutcome> {
    let mut store = Store::open(&config.data_dir)?;
    let recovered = store.recover_incomplete()?;
    if !recovered.is_empty() {
        for id in &recovered {
            println!("recovered  {} (aborted)", short_id(id));
        }
        collector::cleanup_abandoned(&config, &recovered).await;
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
    let mut manifest = RunManifest {
        schema_version: 4,
        id: id.clone(),
        mode,
        state: RunState::Running,
        started_at,
        finished_at: None,
        source,
        tooling,
        benchmark: BenchmarkResult::default(),
        collectors: Vec::new(),
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
        let execution = benchmark::execute(&config, &staging, shutdown.clone()).await;
        manifest.benchmark = execution.result;
        manifest.logs.extend(execution.logs);
        store.checkpoint(&manifest)?;

        let during = collector::stop_during(running, &staging).await;
        absorb(
            during,
            &mut manifest,
            &mut metrics,
            &mut fingerprints,
            &mut transitions,
        );
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

    let collector_degraded = manifest
        .collectors
        .iter()
        .any(|collector| collector.status == "failed");
    manifest.state = match manifest.benchmark.passed {
        _ if manifest.benchmark.interrupted => RunState::Aborted,
        Some(true) if collector_degraded => RunState::Degraded,
        Some(true) => RunState::Complete,
        _ => RunState::Failed,
    };
    manifest.finished_at = Some(Utc::now());
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
    println!("saved     {}", final_dir.display());

    Ok(RunOutcome {
        id,
        passed: manifest.benchmark.passed == Some(true),
        state: manifest.state,
        score: manifest.benchmark.score,
    })
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
