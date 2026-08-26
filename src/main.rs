use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use isuscope::{
    config::LoadedConfig,
    init,
    model::RunMode,
    runner,
    shutdown::Shutdown,
    storage::{RunSummary, Store},
};
use std::{env, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(name = "isuscope", version, about = "ISUCONのベンチ実行記録ツール")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // hidden transition helper keeps its field names explicit
enum Commands {
    /// プロジェクトへ一度だけ使う設定雛形を生成します。
    Init,
    /// 低負荷collectorでベンチを実行します。
    Run,
    /// 詳細調査用collectorを有効にしてベンチを実行します。
    DiscoveryRun,
    /// run一覧、または指定したrunの詳細を表示します。
    Show {
        /// `latest`、完全なrun ID、または一意な短縮IDを指定します。
        run: Option<String>,
    },
    #[command(name = "__transition", hide = true)]
    InternalTransition {
        #[arg(long)]
        run_dir: PathBuf,
        #[arg(long)]
        prefix: String,
        #[arg(long)]
        rules: Option<PathBuf>,
        #[arg(long, default_value = "time")]
        time_field: String,
        #[arg(long, default_value = "session")]
        session_field: String,
        #[arg(long, default_value = "method")]
        method_field: String,
        #[arg(long, default_value = "uri")]
        uri_field: String,
        #[arg(long, default_value = "status")]
        status_field: String,
        #[arg(long, default_value = "reqtime")]
        request_time_field: String,
        #[arg(long, default_value = "apptime")]
        upstream_time_field: String,
        #[arg(long, default_value = "size")]
        bytes_field: String,
        #[arg(long, default_value = "connreqs")]
        connection_requests_field: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(2)
        }
    }
}

async fn real_main() -> Result<bool> {
    let cli = Cli::parse();
    if let Commands::InternalTransition {
        run_dir,
        prefix,
        rules,
        time_field,
        session_field,
        method_field,
        uri_field,
        status_field,
        request_time_field,
        upstream_time_field,
        bytes_field,
        connection_requests_field,
    } = &cli.command
    {
        isuscope::transition::emit(isuscope::transition::TransitionOptions {
            run_dir,
            prefix,
            rules: rules.as_deref(),
            time_field,
            session_field,
            method_field,
            uri_field,
            status_field,
            request_time_field,
            upstream_time_field,
            bytes_field,
            connection_requests_field,
        })?;
        return Ok(true);
    }
    let current = env::current_dir().context("cannot determine current directory")?;
    if matches!(cli.command, Commands::Init) {
        init::scaffold(&current)?;
        return Ok(true);
    }
    let config = LoadedConfig::discover(&current)?;
    match cli.command {
        Commands::Init => unreachable!(),
        Commands::Run => Ok(runner::execute(config, RunMode::Run, Shutdown::listen())
            .await?
            .passed),
        Commands::DiscoveryRun => {
            Ok(
                runner::execute(config, RunMode::DiscoveryRun, Shutdown::listen())
                    .await?
                    .passed,
            )
        }
        Commands::Show { run } => {
            show(&config, run.as_deref())?;
            Ok(true)
        }
        Commands::InternalTransition { .. } => unreachable!(),
    }
}

fn show(config: &LoadedConfig, requested: Option<&str>) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    let Some(requested) = requested else {
        let runs = store.list(20)?;
        if runs.is_empty() {
            println!("no runs");
            return Ok(());
        }
        println!(
            "{:<9}  {:<19}  {:<13}  {:<13}  {:<9}  {:>10}",
            "RUN", "STARTED", "COMMIT", "MODE", "RESULT", "SCORE"
        );
        for run in runs {
            print_summary(&run);
        }
        return Ok(());
    };
    let id = store
        .resolve_id(requested)?
        .with_context(|| format!("run `{requested}` was not found"))?;
    let manifest = store.load(&id)?;
    println!("run         {}", manifest.id);
    println!("started     {}", manifest.started_at.to_rfc3339());
    println!(
        "finished    {}",
        manifest
            .finished_at
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| "-".into())
    );
    println!("mode        {}", manifest.mode.as_str());
    println!("state       {}", manifest.state.as_str());
    println!(
        "commit      {}",
        manifest.source.commit_hash.as_deref().unwrap_or("-")
    );
    println!("dirty       {}", manifest.source.dirty);
    println!("state hash  {}", manifest.source.state_sha256);
    println!("isuscope    {}", manifest.tooling.isuscope_version);
    println!("config hash {}", manifest.tooling.config_sha256);
    println!(
        "score       {}",
        manifest
            .benchmark
            .score
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".into())
    );
    println!(
        "result      {}",
        match manifest.benchmark.passed {
            Some(true) => "PASS",
            Some(false) => "FAIL",
            None => "-",
        }
    );
    println!("collectors  {}", manifest.collectors.len());
    println!("metrics     {}", manifest.metric_count);
    println!("fingerprints {}", manifest.fingerprint_count);
    println!("transitions {}", manifest.transition_count);
    println!("logs        {}", manifest.logs.len());
    for log in &manifest.logs {
        println!("  {}", log.id);
    }
    println!(
        "path        {}",
        runner::run_path(store.data_dir(), &id).display()
    );
    Ok(())
}

fn print_summary(run: &RunSummary) {
    let started = run
        .started_at
        .get(..19)
        .unwrap_or(&run.started_at)
        .replace('T', " ");
    let commit = run
        .commit_hash
        .as_deref()
        .map(|value| &value[..value.len().min(10)])
        .unwrap_or("-");
    let dirty = if run.dirty { "*" } else { "" };
    let result = match run.passed {
        Some(true) => "PASS",
        Some(false) => "FAIL",
        None => run.state.as_str(),
    };
    let score = run
        .score
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".into());
    println!(
        "{:<9}  {:<19}  {:<13}  {:<13}  {:<9}  {:>10}",
        runner::short_id(&run.id),
        started,
        format!("{commit}{dirty}"),
        run.mode,
        result,
        score,
    );
}
