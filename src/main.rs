use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use isuscope::{
    brief,
    config::LoadedConfig,
    diff, doctor, enrichment, init,
    metric_semantics::{self, MetricAggregation},
    model::{AnalysisVerdict, RunManifest, RunMode},
    query::{self, DatabaseQueryOptions, HttpQueryOptions, MetricQueryOptions, QueryScope},
    report::{self, RunDiagnostics, RunReport},
    runner::{self, RunAnnotations},
    shutdown::Shutdown,
    storage::{RunSummary, Store},
};
use std::collections::{BTreeMap, BTreeSet};
use std::{env, fs, path::PathBuf, process::ExitCode};

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
    /// 標準collectorでベンチを実行します。
    Run {
        #[command(flatten)]
        annotations: AnnotationArgs,
    },
    /// 序盤の全体調査として行動遷移分析を加えてベンチを実行します。
    SurveyRun {
        #[command(flatten)]
        annotations: AnnotationArgs,
    },
    /// 保存済みrunを新しい順にJSONで一覧表示します。
    List {
        /// 返すrun数の上限。
        #[arg(long, default_value_t = 20, value_parser = parse_list_limit)]
        limit: usize,
    },
    /// 最新runの人間向けUIをlocalhostで起動します。
    Ui,
    /// 1回のrunを構造化したReport JSONとして出力します。
    Report {
        /// `latest`、run ID、一意な短縮ID、または一意なtagを指定します。
        #[arg(default_value = "latest")]
        run: String,
    },
    /// runの判断材料だけを小さい機械向けJSONで出力します。
    Brief {
        /// `latest`、run ID、一意な短縮ID、または一意なtagを指定します。
        #[arg(default_value = "latest")]
        run: String,
        /// 各性能sectionの上限。
        #[arg(long, default_value_t = 5, value_parser = parse_list_limit)]
        limit: usize,
    },
    /// 2回のrunを全件比較後にcompact化したDiff JSONとして出力します。
    Diff {
        /// 比較基準のrun ID、一意な短縮ID、または一意なtagを指定します。
        base: String,
        /// 比較対象のrun ID、一意な短縮ID、または一意なtagを指定します。
        candidate: String,
    },
    /// metric名、時刻範囲、label cardinalityをJSONで出力します。
    Metrics {
        /// `latest`、run ID、一意な短縮ID、または一意なtagを指定します。
        #[arg(default_value = "latest")]
        run: String,
    },
    /// 時刻付きmetricをbucket化したJSONで出力します。
    Series {
        /// `latest`、run ID、一意な短縮ID、または一意なtagを指定します。
        #[arg(default_value = "latest")]
        run: String,
        /// 返すmetric名。複数回指定できます。指定時は汎用metric行になります。
        #[arg(long = "metric")]
        metrics: Vec<String>,
        /// node labelで絞り込みます。
        #[arg(long)]
        node: Option<String>,
        /// `key=value`形式のlabel完全一致。複数回指定できます。
        #[arg(long = "label", value_parser = parse_label_filter)]
        labels: Vec<(String, String)>,
        /// benchmark開始からの取得開始秒。
        #[arg(long, default_value_t = 0)]
        from: u64,
        /// benchmark開始からの取得終了秒。省略時は終了までです。
        #[arg(long)]
        to: Option<u64>,
        /// benchmark全体、初期化、負荷走行の名前付き区間。
        #[arg(long, value_enum, default_value_t = SeriesWindowArg::Whole)]
        window: SeriesWindowArg,
        /// bucket幅（秒）。
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=3600))]
        bucket: u64,
        /// 出力行数の上限。高cardinalityなperf seriesのJSON肥大化を防ぎます。
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    /// 保存済みmetricをSQLiteから絞り込み、意味に沿って構造化JSONで返します。
    Query {
        /// `latest`、run ID、一意な短縮ID、または一意なtagを指定します。
        #[arg(default_value = "latest")]
        run: String,
        /// 同じselectorを適用して比較する基準run。
        #[arg(long)]
        base: Option<String>,
        /// 汎用metric行またはdatabase集約を選びます。
        #[arg(long, value_enum, default_value_t = QueryViewArg::Metrics)]
        view: QueryViewArg,
        /// run集約値またはtimestamp付きseries値を選びます。
        #[arg(long, value_enum, default_value_t = QueryScopeArg::Run)]
        scope: QueryScopeArg,
        /// series集約に使うbenchmark全体、初期化、負荷走行の区間。
        #[arg(long, value_enum, default_value_t = SeriesWindowArg::Whole)]
        window: SeriesWindowArg,
        /// 返すmetric名。複数回指定できます。
        #[arg(long = "metric")]
        metrics: Vec<String>,
        /// metric名の前方一致。
        #[arg(long)]
        metric_prefix: Option<String>,
        /// node labelで絞り込みます。
        #[arg(long)]
        node: Option<String>,
        /// collectorまたはparser sourceで絞り込みます。
        #[arg(long)]
        source: Option<String>,
        /// `key=value`形式のlabel完全一致。複数回指定できます。
        #[arg(long = "label", value_parser = parse_label_filter)]
        labels: Vec<(String, String)>,
        /// `key=value`形式のlabel部分一致。複数回指定できます。
        #[arg(long = "label-contains", value_parser = parse_label_filter)]
        label_contains: Vec<(String, String)>,
        /// 出力で保持するlabel。provenance labelは常に保持します。
        #[arg(long = "group-by")]
        group_by: Vec<String>,
        /// 出力行数の上限。
        #[arg(long, default_value_t = 100, value_parser = parse_list_limit)]
        limit: usize,
    },
    /// 保存済みbenchmark logへ現在のparserを適用します。
    Enrich {
        /// run ID、一意な短縮ID、または一意なtagを指定します。
        run: String,
    },
    /// ベンチを起動せず、設定・command・SSH・時刻・diskを検査します。
    Doctor,
    /// PASSしたrunへ仮説の判定と結果分析を追記します。
    Analyze {
        /// run ID、一意な短縮ID、または一意なtagを指定します。
        run: String,
        /// 仮説の判定。
        #[arg(value_enum)]
        verdict: VerdictArg,
        /// 結果の分析本文。
        #[arg(long, conflicts_with = "analysis_file")]
        analysis: Option<String>,
        /// 結果の分析本文をUTF-8 fileから読み込みます。
        #[arg(long, conflicts_with = "analysis")]
        analysis_file: Option<PathBuf>,
        /// 分析を省略する理由。
        #[arg(long)]
        reason: Option<String>,
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
        #[arg(long)]
        series_only: bool,
    },
    /// survey-run用のHTTP入出力capture proxyです。
    #[command(name = "__discovery-capture", hide = true)]
    InternalDiscoveryCapture {
        #[arg(long)]
        listen: std::net::SocketAddr,
        #[arg(long)]
        upstream: String,
        #[arg(long, default_value_t = 1_048_576)]
        max_body_bytes: usize,
        #[arg(long)]
        session_cookie: Option<String>,
        #[arg(long, env = "ISUSCOPE_DISCOVERY_SESSION_KEY", hide_env_values = true)]
        session_key: String,
    },
}

#[derive(Debug, Clone, Default, Args)]
struct AnnotationArgs {
    /// 今回の変更がなぜ、どの観測値をどう改善すると考えるかを記録します。
    #[arg(long)]
    hypothesis: String,
    /// runの目的や変更内容を記録します。
    #[arg(long)]
    note: Option<String>,
    /// 検索用tag。複数回指定できます。
    #[arg(long = "tag")]
    tags: Vec<String>,
}

impl From<AnnotationArgs> for RunAnnotations {
    fn from(value: AnnotationArgs) -> Self {
        Self {
            hypothesis: value.hypothesis,
            note: value.note,
            tags: value.tags,
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum VerdictArg {
    Supported,
    Rejected,
    Inconclusive,
    Skipped,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum QueryViewArg {
    Metrics,
    Database,
    Http,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum QueryScopeArg {
    Run,
    Series,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SeriesWindowArg {
    Whole,
    Initialize,
    Load,
}

impl SeriesWindowArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Whole => "whole",
            Self::Initialize => "initialize",
            Self::Load => "load",
        }
    }
}

impl From<QueryScopeArg> for QueryScope {
    fn from(value: QueryScopeArg) -> Self {
        match value {
            QueryScopeArg::Run => Self::Run,
            QueryScopeArg::Series => Self::Series,
        }
    }
}

impl From<VerdictArg> for AnalysisVerdict {
    fn from(value: VerdictArg) -> Self {
        match value {
            VerdictArg::Supported => Self::Supported,
            VerdictArg::Rejected => Self::Rejected,
            VerdictArg::Inconclusive => Self::Inconclusive,
            VerdictArg::Skipped => Self::Skipped,
        }
    }
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
    if let Commands::InternalDiscoveryCapture {
        listen,
        upstream,
        max_body_bytes,
        session_cookie,
        session_key,
    } = &cli.command
    {
        isuscope::discovery_capture::serve(isuscope::discovery_capture::CaptureOptions {
            listen: *listen,
            upstream: upstream.clone(),
            max_body_bytes: *max_body_bytes,
            session_cookie: session_cookie.clone(),
            session_key: session_key.as_bytes().to_vec(),
        })
        .await?;
        return Ok(true);
    }
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
        series_only,
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
            series_only: *series_only,
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
        Commands::InternalDiscoveryCapture { .. } | Commands::InternalTransition { .. } => {
            unreachable!()
        }
        Commands::Run { annotations } => {
            Ok(
                runner::execute(config, RunMode::Run, Shutdown::listen(), annotations.into())
                    .await?
                    .passed,
            )
        }
        Commands::SurveyRun { annotations } => Ok(runner::execute(
            config,
            RunMode::SurveyRun,
            Shutdown::listen(),
            annotations.into(),
        )
        .await?
        .passed),
        Commands::List { limit } => {
            list_runs(&config, limit)?;
            Ok(true)
        }
        Commands::Ui => {
            isuscope::ui::serve(config, Shutdown::listen()).await?;
            Ok(true)
        }
        Commands::Report { run } => {
            show_report(&config, &run)?;
            Ok(true)
        }
        Commands::Brief { run, limit } => {
            show_brief(&config, &run, limit)?;
            Ok(true)
        }
        Commands::Diff { base, candidate } => {
            show_diff(&config, &base, &candidate)?;
            Ok(true)
        }
        Commands::Metrics { run } => {
            show_metrics(&config, &run)?;
            Ok(true)
        }
        Commands::Series {
            run,
            metrics,
            node,
            labels,
            from,
            to,
            window,
            bucket,
            limit,
        } => {
            show_series(
                &config,
                &run,
                SeriesOptions {
                    metrics,
                    node,
                    labels,
                    from,
                    to,
                    window,
                    bucket,
                    limit,
                },
            )?;
            Ok(true)
        }
        Commands::Query {
            run,
            base,
            view,
            scope,
            window,
            metrics,
            metric_prefix,
            node,
            source,
            labels,
            label_contains,
            group_by,
            limit,
        } => {
            show_query(
                &config,
                &run,
                base.as_deref(),
                view,
                scope,
                window,
                metrics,
                metric_prefix,
                node,
                source,
                labels,
                label_contains,
                group_by,
                limit,
            )?;
            Ok(true)
        }
        Commands::Enrich { run } => {
            let outcome = enrichment::enrich_saved(&config, &run).await?;
            println!("run       {}", runner::short_id(&outcome.run_id));
            println!("parsers   {}", outcome.parser_count);
            println!("metrics   {}", outcome.metric_count);
            println!(
                "result    {}",
                if outcome.failed {
                    "DEGRADED"
                } else {
                    "COMPLETE"
                }
            );
            Ok(!outcome.failed)
        }
        Commands::Doctor => {
            let report = doctor::run(&config).await?;
            println!();
            println!("passed    {}", report.passed);
            println!("warnings  {}", report.warnings.len());
            println!("failures  {}", report.failures.len());
            Ok(report.healthy())
        }
        Commands::Analyze {
            run,
            verdict,
            analysis,
            analysis_file,
            reason,
        } => {
            let body = if matches!(verdict, VerdictArg::Skipped) {
                if analysis.is_some() || analysis_file.is_some() {
                    anyhow::bail!(
                        "the skipped verdict cannot be combined with --analysis or --analysis-file"
                    );
                }
                reason
                    .filter(|value| !value.trim().is_empty())
                    .context("the skipped verdict requires a non-empty --reason")?
            } else {
                if reason.is_some() {
                    anyhow::bail!("--reason may be used only with the skipped verdict");
                }
                let body = match (analysis, analysis_file) {
                    (Some(body), None) => body,
                    (None, Some(path)) => fs::read_to_string(&path)
                        .with_context(|| format!("cannot read {}", path.display()))?,
                    (None, None) => anyhow::bail!(
                        "analysis requires either --analysis <text> or --analysis-file <path>"
                    ),
                    (Some(_), Some(_)) => unreachable!("clap enforces conflicting arguments"),
                };
                if body.trim().is_empty() {
                    anyhow::bail!("analysis must not be empty");
                }
                body
            };
            let mut store = Store::open(&config.data_dir)?;
            let id = store
                .resolve_id(&run)?
                .with_context(|| format!("run `{run}` was not found"))?;
            let manifest = store.append_analysis(&id, verdict.into(), body)?;
            let latest = manifest
                .analyses
                .last()
                .context("analysis was not appended")?;
            println!("run       {}", runner::short_id(&manifest.id));
            println!("verdict   {}", latest.verdict.as_str());
            println!("analysis  {}", manifest.analysis_status.as_str());
            println!("revisions {}", manifest.analyses.len());
            Ok(true)
        }
    }
}

#[derive(Default)]
struct BucketRow {
    cpu_host_sampler: Vec<f64>,
    cpu_sysstat: Vec<f64>,
    cpu_other: Vec<f64>,
    memory: Vec<f64>,
    load: Vec<f64>,
    disk_util: Vec<f64>,
    disk_await: Vec<f64>,
    http_requests: f64,
    http_p95: Vec<f64>,
    http_errors: f64,
    db_calls: f64,
    db_time: f64,
}

#[derive(Debug)]
struct SeriesOptions {
    metrics: Vec<String>,
    node: Option<String>,
    labels: Vec<(String, String)>,
    from: u64,
    to: Option<u64>,
    window: SeriesWindowArg,
    bucket: u64,
    limit: usize,
}

#[derive(serde::Serialize)]
struct SeriesOutput {
    schema_version: u32,
    run_id: String,
    benchmark: SeriesInterval,
    window: SeriesWindow,
    filters: SeriesFilters,
    coverage: Vec<SeriesCoverage>,
    #[serde(flatten)]
    data: SeriesData,
}

#[derive(serde::Serialize)]
struct SeriesInterval {
    started_at: String,
    finished_at: String,
}

#[derive(serde::Serialize)]
struct SeriesWindow {
    name: String,
    started_at: String,
    finished_at: String,
    from_seconds: i64,
    to_seconds: i64,
    bucket_seconds: u64,
}

#[derive(serde::Serialize)]
struct SeriesFilters {
    metrics: Vec<String>,
    node: Option<String>,
    labels: Vec<SeriesLabelFilter>,
}

#[derive(serde::Serialize)]
struct SeriesLabelFilter {
    key: String,
    value: String,
}

#[derive(serde::Serialize)]
struct SeriesCoverage {
    collector: String,
    node: String,
    phase: String,
    status: String,
    exit_code: Option<i32>,
    error: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum SeriesData {
    Overview {
        total_count: usize,
        truncated: bool,
        rows: Vec<OverviewSeriesRow>,
    },
    Metrics {
        total_count: usize,
        truncated: bool,
        rows: Vec<MetricSeriesRow>,
    },
}

#[derive(serde::Serialize)]
struct OverviewSeriesRow {
    node: String,
    from_seconds: i64,
    to_seconds: i64,
    cpu_percent_average: Option<f64>,
    cpu_percent_max: Option<f64>,
    memory_used_mib_average: Option<f64>,
    load1_max: Option<f64>,
    disk_util_percent_max: Option<f64>,
    disk_await_ms_max: Option<f64>,
    http_requests: Option<f64>,
    http_p95_ms_max_of_quantile: Option<f64>,
    http_errors: Option<f64>,
    db_calls: Option<f64>,
    db_total_duration_ms: Option<f64>,
}

#[derive(serde::Serialize)]
struct MetricSeriesRow {
    node: String,
    from_seconds: i64,
    to_seconds: i64,
    metric: String,
    value: f64,
    unit: String,
    aggregation: MetricAggregation,
    labels: BTreeMap<String, String>,
}

fn parse_list_limit(value: &str) -> std::result::Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| "limit must be an integer from 1 to 1000".to_string())?;
    if !(1..=1000).contains(&limit) {
        return Err("limit must be an integer from 1 to 1000".into());
    }
    Ok(limit)
}

fn parse_label_filter(value: &str) -> std::result::Result<(String, String), String> {
    let Some((key, value)) = value.split_once('=') else {
        return Err("label must use key=value syntax".into());
    };
    if key.is_empty() || value.is_empty() {
        return Err("label key and value must not be empty".into());
    }
    Ok((key.into(), value.into()))
}

#[derive(Default)]
struct MetricInventory {
    rows: usize,
    timestamped: usize,
    units: BTreeSet<String>,
    first: Option<chrono::DateTime<chrono::Utc>>,
    last: Option<chrono::DateTime<chrono::Utc>>,
    labels: BTreeMap<String, BTreeSet<String>>,
}

#[derive(serde::Serialize)]
struct MetricsOutput {
    schema_version: u32,
    run_id: String,
    metrics: Vec<MetricInventoryOutput>,
}

#[derive(serde::Serialize)]
struct MetricInventoryOutput {
    name: String,
    rows: usize,
    timestamped_rows: usize,
    units: Vec<String>,
    first_observed_at: Option<String>,
    last_observed_at: Option<String>,
    labels: Vec<LabelInventoryOutput>,
}

#[derive(serde::Serialize)]
struct LabelInventoryOutput {
    key: String,
    cardinality: usize,
    examples: Vec<String>,
}

fn show_metrics(config: &LoadedConfig, requested: &str) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    let id = store
        .resolve_id(requested)?
        .with_context(|| format!("run `{requested}` was not found"))?;
    let mut inventory = BTreeMap::<String, MetricInventory>::new();
    for metric in store.metrics(&id)? {
        let entry = inventory.entry(metric.name).or_default();
        entry.rows += 1;
        entry.units.insert(metric.unit);
        if let Some(at) = metric.timestamp {
            entry.timestamped += 1;
            entry.first = Some(entry.first.map_or(at, |current| current.min(at)));
            entry.last = Some(entry.last.map_or(at, |current| current.max(at)));
        }
        for (key, value) in metric.labels {
            entry.labels.entry(key).or_default().insert(value);
        }
    }
    let metrics = inventory
        .into_iter()
        .map(|(name, item)| MetricInventoryOutput {
            name,
            rows: item.rows,
            timestamped_rows: item.timestamped,
            units: item.units.into_iter().collect(),
            first_observed_at: item.first.map(|value| value.to_rfc3339()),
            last_observed_at: item.last.map(|value| value.to_rfc3339()),
            labels: item
                .labels
                .into_iter()
                .map(|(key, values)| LabelInventoryOutput {
                    key,
                    cardinality: values.len(),
                    examples: values.into_iter().take(4).collect(),
                })
                .collect(),
        })
        .collect();
    write_stdout_json(&MetricsOutput {
        schema_version: 1,
        run_id: id,
        metrics,
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn show_query(
    config: &LoadedConfig,
    requested: &str,
    base_requested: Option<&str>,
    view: QueryViewArg,
    scope: QueryScopeArg,
    window: SeriesWindowArg,
    metrics: Vec<String>,
    metric_prefix: Option<String>,
    node: Option<String>,
    source: Option<String>,
    labels: Vec<(String, String)>,
    label_contains: Vec<(String, String)>,
    group_by: Vec<String>,
    limit: usize,
) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    let id = store
        .resolve_id(requested)?
        .with_context(|| format!("run `{requested}` was not found"))?;
    let base_id = base_requested
        .map(|requested| {
            store
                .resolve_id(requested)?
                .with_context(|| format!("base run `{requested}` was not found"))
        })
        .transpose()?;
    let query_limit = base_id.as_ref().map_or(limit, |_| usize::MAX);
    if !matches!(scope, QueryScopeArg::Series) && window != SeriesWindowArg::Whole {
        bail!("--window initialize/load requires `--scope series`");
    }
    match view {
        QueryViewArg::Metrics => {
            let mut candidate_metrics = store.query_metrics(
                &id,
                &metrics,
                metric_prefix.as_deref(),
                Some(matches!(scope, QueryScopeArg::Series)),
            )?;
            if matches!(scope, QueryScopeArg::Series) {
                let manifest = store.load(&id)?;
                let (start, end) = named_window(&manifest, window)?;
                candidate_metrics.retain(|metric| {
                    metric
                        .timestamp
                        .is_some_and(|timestamp| timestamp >= start && timestamp <= end)
                });
            }
            let options = MetricQueryOptions {
                scope: scope.into(),
                window: matches!(scope, QueryScopeArg::Series).then(|| window.as_str().to_string()),
                metrics,
                metric_prefix,
                node,
                source,
                labels,
                label_contains,
                group_by,
                limit: query_limit,
            };
            let candidate = query::metric_query(id, candidate_metrics, options.clone());
            if let Some(base_id) = base_id {
                let mut base_metrics = store.query_metrics(
                    &base_id,
                    &options.metrics,
                    options.metric_prefix.as_deref(),
                    Some(options.scope == QueryScope::Series),
                )?;
                if options.scope == QueryScope::Series {
                    let manifest = store.load(&base_id)?;
                    let (start, end) = named_window(&manifest, window)?;
                    base_metrics.retain(|metric| {
                        metric
                            .timestamp
                            .is_some_and(|timestamp| timestamp >= start && timestamp <= end)
                    });
                }
                let base = query::metric_query(base_id, base_metrics, options);
                write_stdout_json(&query::metric_query_diff(base, candidate, limit))?;
            } else {
                write_stdout_json(&candidate)?;
            }
        }
        QueryViewArg::Database => {
            if !matches!(scope, QueryScopeArg::Run) {
                bail!("database view currently supports only --scope run");
            }
            if !metrics.is_empty() || metric_prefix.is_some() {
                bail!("database view selects its metric set; remove --metric/--metric-prefix");
            }
            if group_by.iter().any(|value| value != "sql-shape") || group_by.len() > 1 {
                bail!("database view supports only `--group-by sql-shape`");
            }
            let candidate_metrics =
                store.query_metrics(&id, &[], Some("db.query."), Some(false))?;
            let options = DatabaseQueryOptions {
                node,
                source,
                labels,
                label_contains,
                sql_shape: group_by.first().is_some_and(|value| value == "sql-shape"),
                limit: query_limit,
            };
            let candidate = query::database_query(id, candidate_metrics, options.clone());
            if let Some(base_id) = base_id {
                let base_metrics =
                    store.query_metrics(&base_id, &[], Some("db.query."), Some(false))?;
                let base = query::database_query(base_id, base_metrics, options);
                write_stdout_json(&query::database_query_diff(base, candidate, limit))?;
            } else {
                write_stdout_json(&candidate)?;
            }
        }
        QueryViewArg::Http => {
            if !matches!(scope, QueryScopeArg::Run) {
                bail!("http view currently supports only --scope run");
            }
            if !metrics.is_empty() || metric_prefix.is_some() {
                bail!("http view selects its metric set; remove --metric/--metric-prefix");
            }
            if !group_by.is_empty() {
                bail!("http view already groups by node, method and route; remove --group-by");
            }
            let candidate_metrics = store.query_metrics(&id, &[], Some("http."), Some(false))?;
            let options = HttpQueryOptions {
                node,
                source,
                labels,
                label_contains,
                limit: query_limit,
            };
            let candidate = query::http_query(id, candidate_metrics, options.clone());
            if let Some(base_id) = base_id {
                let base_metrics =
                    store.query_metrics(&base_id, &[], Some("http."), Some(false))?;
                let base = query::http_query(base_id, base_metrics, options);
                write_stdout_json(&query::http_query_diff(base, candidate, limit))?;
            } else {
                write_stdout_json(&candidate)?;
            }
        }
    }
    Ok(())
}

fn show_series(config: &LoadedConfig, requested: &str, options: SeriesOptions) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    let id = store
        .resolve_id(requested)?
        .with_context(|| format!("run `{requested}` was not found"))?;
    let manifest = store.load(&id)?;
    let start = manifest.benchmark.started_at.unwrap_or(manifest.started_at);
    let end = manifest
        .benchmark
        .finished_at
        .or(manifest.finished_at)
        .unwrap_or(start);
    if options.window != SeriesWindowArg::Whole && (options.from != 0 || options.to.is_some()) {
        bail!("--from/--to can be used only with `--window whole`");
    }
    let (requested_start, requested_end) = match options.window {
        SeriesWindowArg::Whole => {
            let requested_end = options
                .to
                .map(|seconds| start + chrono::Duration::seconds(seconds as i64))
                .unwrap_or(end)
                .min(end);
            (
                (start + chrono::Duration::seconds(options.from as i64)).min(requested_end),
                requested_end,
            )
        }
        SeriesWindowArg::Initialize => (
            manifest
                .benchmark
                .initialize_started_at
                .context("run has no initialize-started checkpoint")?,
            manifest
                .benchmark
                .initialize_finished_at
                .context("run has no initialize-finished checkpoint")?,
        ),
        SeriesWindowArg::Load => (
            manifest
                .benchmark
                .initialize_finished_at
                .context("run has no initialize-finished checkpoint")?,
            end,
        ),
    };
    let bucket_seconds = options.bucket as i64;
    let duration = (end - start).num_seconds().max(0);
    let metrics = store
        .metrics(&id)?
        .into_iter()
        .filter(|metric| metric_matches(metric, &options, requested_start, requested_end))
        .collect::<Vec<_>>();
    if !options.metrics.is_empty() {
        let data = generic_series_data(start, requested_start, requested_end, &options, metrics);
        write_stdout_json(&series_output(
            id,
            start,
            end,
            requested_start,
            requested_end,
            &options,
            series_coverage(&manifest.collectors),
            data,
        ))?;
        return Ok(());
    }
    let mut rows = BTreeMap::<(String, i64), BucketRow>::new();
    let mut nodes = BTreeSet::new();
    for metric in metrics {
        let Some(at) = metric.timestamp else { continue };
        let bucket = (at.timestamp() - requested_start.timestamp()).div_euclid(bucket_seconds);
        let node = metric
            .labels
            .get("node")
            .cloned()
            .unwrap_or_else(|| "local".into());
        nodes.insert(node.clone());
        let row = rows.entry((node, bucket)).or_default();
        match metric.name.as_str() {
            "host.cpu_busy_percent" | "host.cpu_percent" => {
                match metric.labels.get("collector").map(String::as_str) {
                    Some("host-sampler") => row.cpu_host_sampler.push(metric.value),
                    Some("sysstat") => row.cpu_sysstat.push(metric.value),
                    _ => row.cpu_other.push(metric.value),
                }
            }
            "host.memory_used_bytes" => row.memory.push(metric.value / 1_048_576.0),
            "host.load1" => row.load.push(metric.value),
            "host.disk_util_percent" => row.disk_util.push(metric.value),
            "host.disk_await" => row.disk_await.push(metric.value),
            "http.requests" => row.http_requests += metric.value,
            "http.errors" => row.http_errors += metric.value,
            "http.request_duration"
                if metric.labels.get("quantile").map(String::as_str) == Some("0.95") =>
            {
                row.http_p95.push(metric.value);
            }
            "db.query.calls" => row.db_calls += metric.value,
            "db.query.total_duration" => row.db_time += metric.value,
            _ => {}
        }
    }
    let window_duration = (requested_end - requested_start).num_seconds().max(0);
    for node in nodes {
        for bucket in 0..=window_duration.div_euclid(bucket_seconds) {
            rows.entry((node.clone(), bucket)).or_default();
        }
    }
    let rows = rows
        .into_iter()
        .map(|((node, bucket), row)| {
            let bucket_offset = (requested_start - start).num_seconds() + bucket * bucket_seconds;
            let from = bucket_offset.max(0);
            let to = (bucket_offset + bucket_seconds).min(duration.max(1));
            OverviewSeriesRow {
                node,
                from_seconds: from,
                to_seconds: to,
                cpu_percent_average: average_value(preferred_cpu(&row)),
                cpu_percent_max: maximum_value(preferred_cpu(&row)),
                memory_used_mib_average: average_value(&row.memory),
                load1_max: maximum_value(&row.load),
                disk_util_percent_max: maximum_value(&row.disk_util),
                disk_await_ms_max: maximum_value(&row.disk_await),
                http_requests: observed_sum(
                    row.http_requests,
                    !row.http_p95.is_empty() || row.http_errors != 0.0,
                ),
                http_p95_ms_max_of_quantile: maximum_value(&row.http_p95),
                http_errors: observed_sum(row.http_errors, row.http_requests != 0.0),
                db_calls: observed_sum(row.db_calls, row.db_time != 0.0),
                db_total_duration_ms: observed_sum(row.db_time, row.db_calls != 0.0),
            }
        })
        .collect::<Vec<_>>();
    let total_count = rows.len();
    write_stdout_json(&series_output(
        id,
        start,
        end,
        requested_start,
        requested_end,
        &options,
        series_coverage(&manifest.collectors),
        SeriesData::Overview {
            total_count,
            truncated: false,
            rows,
        },
    ))?;
    Ok(())
}

fn named_window(
    manifest: &RunManifest,
    window: SeriesWindowArg,
) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    let benchmark_start = manifest.benchmark.started_at.unwrap_or(manifest.started_at);
    let benchmark_end = manifest
        .benchmark
        .finished_at
        .or(manifest.finished_at)
        .unwrap_or(benchmark_start);
    let bounds = match window {
        SeriesWindowArg::Whole => (benchmark_start, benchmark_end),
        SeriesWindowArg::Initialize => (
            manifest
                .benchmark
                .initialize_started_at
                .context("run has no initialize-started checkpoint")?,
            manifest
                .benchmark
                .initialize_finished_at
                .context("run has no initialize-finished checkpoint")?,
        ),
        SeriesWindowArg::Load => (
            manifest
                .benchmark
                .initialize_finished_at
                .context("run has no initialize-finished checkpoint")?,
            benchmark_end,
        ),
    };
    if bounds.0 > bounds.1 {
        bail!("run has an invalid {} window", window.as_str());
    }
    Ok(bounds)
}

#[allow(clippy::too_many_arguments)]
fn series_output(
    run_id: String,
    benchmark_start: chrono::DateTime<chrono::Utc>,
    benchmark_end: chrono::DateTime<chrono::Utc>,
    window_start: chrono::DateTime<chrono::Utc>,
    window_end: chrono::DateTime<chrono::Utc>,
    options: &SeriesOptions,
    coverage: Vec<SeriesCoverage>,
    data: SeriesData,
) -> SeriesOutput {
    SeriesOutput {
        schema_version: 1,
        run_id,
        benchmark: SeriesInterval {
            started_at: benchmark_start.to_rfc3339(),
            finished_at: benchmark_end.to_rfc3339(),
        },
        window: SeriesWindow {
            name: options.window.as_str().into(),
            started_at: window_start.to_rfc3339(),
            finished_at: window_end.to_rfc3339(),
            from_seconds: (window_start - benchmark_start).num_seconds(),
            to_seconds: (window_end - benchmark_start).num_seconds(),
            bucket_seconds: options.bucket,
        },
        filters: SeriesFilters {
            metrics: options.metrics.clone(),
            node: options.node.clone(),
            labels: options
                .labels
                .iter()
                .map(|(key, value)| SeriesLabelFilter {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
        },
        coverage,
        data,
    }
}

fn metric_matches(
    metric: &isuscope::model::Metric,
    options: &SeriesOptions,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> bool {
    if !options.metrics.is_empty() && !options.metrics.contains(&metric.name) {
        return false;
    }
    if let Some(node) = &options.node
        && metric.labels.get("node") != Some(node)
    {
        return false;
    }
    if options
        .labels
        .iter()
        .any(|(key, value)| metric.labels.get(key) != Some(value))
    {
        return false;
    }
    metric.timestamp.is_some_and(|at| at >= start && at <= end)
}

fn generic_series_data(
    benchmark_start: chrono::DateTime<chrono::Utc>,
    window_start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    options: &SeriesOptions,
    metrics: Vec<isuscope::model::Metric>,
) -> SeriesData {
    type SeriesKey = (String, i64, String, String, BTreeMap<String, String>);
    let mut rows = BTreeMap::<SeriesKey, (MetricAggregation, Vec<f64>)>::new();
    let bucket_seconds = options.bucket as i64;
    for metric in metrics {
        let Some(at) = metric.timestamp else { continue };
        let node = metric
            .labels
            .get("node")
            .cloned()
            .unwrap_or_else(|| "local".into());
        let bucket = (at.timestamp() - window_start.timestamp()).div_euclid(bucket_seconds);
        let labels = metric
            .labels
            .iter()
            .filter(|(key, _)| key.as_str() != "node")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        let aggregation = metric_semantics::time_series_aggregation(&metric);
        rows.entry((node, bucket, metric.name, metric.unit, labels))
            .or_insert_with(|| (aggregation, Vec::new()))
            .1
            .push(metric.value);
    }
    let total_count = rows.len();
    let rows = rows
        .into_iter()
        .take(options.limit)
        .map(
            |((node, bucket, metric, unit, labels), (aggregation, values))| {
                let value = metric_semantics::aggregate(&values, aggregation).unwrap_or_default();
                let from_seconds = ((window_start - benchmark_start).num_seconds()
                    + bucket * bucket_seconds)
                    .max(0);
                let to_seconds = (from_seconds + bucket_seconds)
                    .min((end - benchmark_start).num_seconds().max(1));
                MetricSeriesRow {
                    node,
                    from_seconds,
                    to_seconds,
                    metric,
                    value,
                    unit,
                    aggregation,
                    labels,
                }
            },
        )
        .collect();
    SeriesData::Metrics {
        total_count,
        truncated: total_count > options.limit,
        rows,
    }
}

fn preferred_cpu(row: &BucketRow) -> &[f64] {
    if !row.cpu_host_sampler.is_empty() {
        &row.cpu_host_sampler
    } else if !row.cpu_sysstat.is_empty() {
        &row.cpu_sysstat
    } else {
        &row.cpu_other
    }
}

fn series_coverage(collectors: &[isuscope::model::CollectorResult]) -> Vec<SeriesCoverage> {
    const SERIES_COLLECTORS: [&str; 7] = [
        "host-sampler",
        "sysstat",
        "service-sampler",
        "nginx-log-delta",
        "nginx-series",
        "mysql-log-delta",
        "perf-series",
    ];
    collectors
        .iter()
        .filter(|collector| SERIES_COLLECTORS.contains(&collector.name.as_str()))
        .map(|collector| SeriesCoverage {
            collector: collector.name.clone(),
            node: collector.node.clone().unwrap_or_else(|| "local".into()),
            phase: collector.phase.clone(),
            status: collector.status.clone(),
            exit_code: collector.exit_code,
            error: collector.error.clone(),
        })
        .collect()
}

fn average_value(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn maximum_value(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::max)
}

fn observed_sum(value: f64, related_observed: bool) -> Option<f64> {
    (value != 0.0 || related_observed).then_some(value)
}

#[derive(serde::Serialize)]
struct RunListOutput {
    schema_version: u32,
    runs: Vec<RunSummary>,
}

fn list_runs(config: &LoadedConfig, limit: usize) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    write_stdout_json(&RunListOutput {
        schema_version: 1,
        runs: store.list(limit)?,
    })?;
    Ok(())
}

fn write_stdout_json(value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout().lock(), value)?;
    println!();
    Ok(())
}

fn show_report(config: &LoadedConfig, requested: &str) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    let report = load_report(config, &store, requested)?;
    write_stdout_json(&report)?;
    Ok(())
}

fn show_brief(config: &LoadedConfig, requested: &str, limit: usize) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    let diagnostics = load_diagnostics(config, &store, requested)?;
    let id = diagnostics.run.id.clone();
    let benchmark_metrics = store.query_metrics(&id, &[], Some("benchmark."), Some(false))?;
    let benchmark = query::metric_query(
        id,
        benchmark_metrics,
        MetricQueryOptions {
            scope: QueryScope::Run,
            window: None,
            metrics: Vec::new(),
            metric_prefix: Some("benchmark.".into()),
            node: None,
            source: None,
            labels: Vec::new(),
            label_contains: Vec::new(),
            group_by: Vec::new(),
            limit: usize::MAX,
        },
    );
    write_stdout_json(&brief::build(diagnostics, benchmark, limit))?;
    Ok(())
}

fn load_report(config: &LoadedConfig, store: &Store, requested: &str) -> Result<RunReport> {
    Ok(load_diagnostics(config, store, requested)?.into_report())
}

fn show_diff(config: &LoadedConfig, base: &str, candidate: &str) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    let base = load_diagnostics(config, &store, base)?;
    let candidate = load_diagnostics(config, &store, candidate)?;
    write_stdout_json(&diff::build(base, candidate))?;
    Ok(())
}

fn load_diagnostics(
    config: &LoadedConfig,
    store: &Store,
    requested: &str,
) -> Result<RunDiagnostics> {
    let id = store
        .resolve_id(requested)?
        .with_context(|| format!("run `{requested}` was not found"))?;
    let latest_logs = (store.resolve_id("latest")?.as_deref() == Some(id.as_str()))
        .then(|| config.data_dir.join("latest/logs"));
    Ok(report::diagnose(
        store.load(&id)?,
        store.metrics(&id)?,
        store.transitions(&id)?,
        store.final_dir(&id).join("logs"),
        latest_logs,
    ))
}

#[cfg(test)]
mod series_tests {
    use super::*;

    #[test]
    fn host_sampler_cpu_wins_over_sysstat() {
        let row = BucketRow {
            cpu_host_sampler: vec![80.0],
            cpu_sysstat: vec![10.0],
            cpu_other: vec![20.0],
            ..Default::default()
        };
        assert_eq!(preferred_cpu(&row), &[80.0]);
    }

    #[test]
    fn database_rows_are_summed_when_rebucketed() {
        let start = chrono::DateTime::from_timestamp(0, 0).unwrap();
        let metrics = [10.0, 20.0]
            .into_iter()
            .enumerate()
            .map(|(index, value)| isuscope::model::Metric {
                name: "db.query.rows_examined".into(),
                value,
                unit: "rows".into(),
                timestamp: Some(start + chrono::Duration::seconds(index as i64 * 5)),
                labels: BTreeMap::from([("digest".into(), "select ?".into())]),
            })
            .collect();
        let data = generic_series_data(
            start,
            start,
            start + chrono::Duration::seconds(60),
            &SeriesOptions {
                metrics: vec!["db.query.rows_examined".into()],
                node: None,
                labels: Vec::new(),
                from: 0,
                to: None,
                window: SeriesWindowArg::Whole,
                bucket: 3600,
                limit: 100,
            },
            metrics,
        );
        let SeriesData::Metrics { rows, .. } = data else {
            panic!("expected metric rows")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, 30.0);
        assert_eq!(rows[0].aggregation, MetricAggregation::Sum);
    }
}
