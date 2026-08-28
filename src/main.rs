use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use isuscope::{
    bottleneck,
    config::LoadedConfig,
    doctor, enrichment, init,
    model::{AnalysisVerdict, RunMode},
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
    /// 標準collectorに行動遷移分析を加えてベンチを実行します。
    DiscoveryRun {
        #[command(flatten)]
        annotations: AnnotationArgs,
    },
    /// collectorを起動せず、スコア取得だけを行います。
    ScoreRun {
        #[command(flatten)]
        annotations: AnnotationArgs,
    },
    /// run一覧、または指定したrunの詳細を表示します。
    Show {
        /// `latest`、run ID、一意な短縮ID、または一意なtagを指定します。
        run: Option<String>,
    },
    /// 指定したrunでカテゴリ別のボトルネック候補を最大5件表示します。
    Bottleneck {
        /// `latest`、完全なrun ID、または一意な短縮IDを指定します。
        #[arg(default_value = "latest")]
        run: String,
    },
    /// metric名、時刻範囲、label cardinalityを一覧表示します。
    Metrics {
        /// `latest`、完全なrun ID、または一意な短縮IDを指定します。
        #[arg(default_value = "latest")]
        run: String,
    },
    /// 時刻付きmetricをbucket化した表で表示します。
    Series {
        /// `latest`、完全なrun ID、または一意な短縮IDを指定します。
        #[arg(default_value = "latest")]
        run: String,
        /// 表示するmetric名。複数回指定できます。指定時は汎用metric表になります。
        #[arg(long = "metric")]
        metrics: Vec<String>,
        /// node labelで絞り込みます。
        #[arg(long)]
        node: Option<String>,
        /// `key=value`形式のlabel完全一致。複数回指定できます。
        #[arg(long = "label", value_parser = parse_label_filter)]
        labels: Vec<(String, String)>,
        /// benchmark開始からの表示開始秒。
        #[arg(long, default_value_t = 0)]
        from: u64,
        /// benchmark開始からの表示終了秒。省略時は終了までです。
        #[arg(long)]
        to: Option<u64>,
        /// bucket幅（秒）。
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=3600))]
        bucket: u64,
        /// 出力行数の上限。高cardinalityなperf seriesの端末氾濫を防ぎます。
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    /// 保存済みbenchmark logへ現在のparserを適用します。
    Enrich {
        /// `latest`、run ID、または一意なtagを指定します。
        #[arg(default_value = "latest")]
        run: String,
    },
    /// 保存済みrunへnoteとtagを追加・更新します。
    Annotate {
        /// `latest`、run ID、または一意なtagを指定します。
        run: String,
        /// runの説明。空文字を指定すると削除します。
        #[arg(long)]
        note: Option<String>,
        /// 追加するtag。複数回指定できます。
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// 削除するtag。複数回指定できます。
        #[arg(long = "remove-tag")]
        remove_tags: Vec<String>,
    },
    /// ベンチを起動せず、設定・command・SSH・時刻・diskを検査します。
    Doctor,
    /// PASSしたrunへ仮説の判定と結果分析を追記します。
    Analyze {
        /// `latest`、run ID、または一意なtagを指定します。
        #[arg(default_value = "latest")]
        run: String,
        /// 仮説の判定。
        #[arg(long, value_enum)]
        verdict: Option<VerdictArg>,
        /// 結果の分析本文。
        #[arg(long, conflicts_with = "analysis_file")]
        analysis: Option<String>,
        /// 結果の分析本文をUTF-8 fileから読み込みます。
        #[arg(long, conflicts_with = "analysis")]
        analysis_file: Option<PathBuf>,
        /// 分析を省略します。理由の記録が必須です。
        #[arg(long)]
        skip: bool,
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
}

impl From<VerdictArg> for AnalysisVerdict {
    fn from(value: VerdictArg) -> Self {
        match value {
            VerdictArg::Supported => Self::Supported,
            VerdictArg::Rejected => Self::Rejected,
            VerdictArg::Inconclusive => Self::Inconclusive,
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
        Commands::Run { annotations } => {
            Ok(
                runner::execute(config, RunMode::Run, Shutdown::listen(), annotations.into())
                    .await?
                    .passed,
            )
        }
        Commands::DiscoveryRun { annotations } => Ok(runner::execute(
            config,
            RunMode::DiscoveryRun,
            Shutdown::listen(),
            annotations.into(),
        )
        .await?
        .passed),
        Commands::ScoreRun { annotations } => Ok(runner::execute(
            config,
            RunMode::ScoreRun,
            Shutdown::listen(),
            annotations.into(),
        )
        .await?
        .passed),
        Commands::Show { run } => {
            show(&config, run.as_deref())?;
            Ok(true)
        }
        Commands::Bottleneck { run } => {
            show_bottlenecks(&config, &run)?;
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
                    bucket,
                    limit,
                },
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
        Commands::Annotate {
            run,
            note,
            tags,
            remove_tags,
        } => {
            let mut store = Store::open(&config.data_dir)?;
            let id = store
                .resolve_id(&run)?
                .with_context(|| format!("run `{run}` was not found"))?;
            let manifest = store.annotate(&id, note, &tags, &remove_tags)?;
            println!("run       {}", runner::short_id(&manifest.id));
            println!("note      {}", manifest.note.as_deref().unwrap_or("-"));
            println!(
                "tags      {}",
                if manifest.tags.is_empty() {
                    "-".into()
                } else {
                    manifest.tags.join(",")
                }
            );
            Ok(true)
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
            skip,
            reason,
        } => {
            let (verdict, body) = if skip {
                if verdict.is_some() || analysis.is_some() || analysis_file.is_some() {
                    anyhow::bail!(
                        "--skip cannot be combined with --verdict, --analysis, or --analysis-file"
                    );
                }
                let reason = reason
                    .filter(|value| !value.trim().is_empty())
                    .context("--skip requires a non-empty --reason")?;
                (AnalysisVerdict::Skipped, reason)
            } else {
                if reason.is_some() {
                    anyhow::bail!("--reason may be used only with --skip");
                }
                let verdict = verdict
                    .context("analysis requires --verdict <supported|rejected|inconclusive>")?;
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
                (verdict.into(), body)
            };
            let mut store = Store::open(&config.data_dir)?;
            let id = store
                .resolve_id(&run)?
                .with_context(|| format!("run `{run}` was not found"))?;
            let manifest = store.append_analysis(&id, verdict, body)?;
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
        Commands::InternalTransition { .. } => unreachable!(),
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
    bucket: u64,
    limit: usize,
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
    println!("run {id}");
    println!(
        "{:<34} {:>8} {:>8}  {:<16}  {:<20}",
        "NAME", "ROWS", "TIMED", "UNIT", "TIME RANGE"
    );
    for (name, item) in &inventory {
        let range = match (item.first, item.last) {
            (Some(first), Some(last)) => format!("{}..{}", first.to_rfc3339(), last.to_rfc3339()),
            _ => "aggregate-only".into(),
        };
        println!(
            "{:<34} {:>8} {:>8}  {:<16}  {}",
            name,
            item.rows,
            item.timestamped,
            item.units.iter().cloned().collect::<Vec<_>>().join("|"),
            range
        );
        for (key, values) in &item.labels {
            let examples = values
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "  label {:<18} values={:<6} examples={}",
                key,
                values.len(),
                examples
            );
        }
    }
    if inventory.is_empty() {
        println!("no metrics");
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
    let requested_end = options
        .to
        .map(|seconds| start + chrono::Duration::seconds(seconds as i64))
        .unwrap_or(end)
        .min(end);
    let requested_start =
        (start + chrono::Duration::seconds(options.from as i64)).min(requested_end);
    let bucket_seconds = options.bucket as i64;
    let first_bucket = requested_start.timestamp().div_euclid(bucket_seconds) * bucket_seconds;
    let duration = (end - start).num_seconds().max(0);
    let metrics = store
        .metrics(&id)?
        .into_iter()
        .filter(|metric| metric_matches(metric, &options, first_bucket, requested_end))
        .collect::<Vec<_>>();
    if !options.metrics.is_empty() {
        return show_generic_series(&id, start, requested_end, &options, metrics);
    }
    let mut rows = BTreeMap::<(String, i64), BucketRow>::new();
    let mut nodes = BTreeSet::new();
    for metric in metrics {
        let Some(at) = metric.timestamp else { continue };
        let bucket = at.timestamp().div_euclid(bucket_seconds) * bucket_seconds;
        let node = metric
            .labels
            .get("node")
            .cloned()
            .unwrap_or_else(|| "local".into());
        nodes.insert(node.clone());
        let row = rows.entry((node, bucket)).or_default();
        match metric.name.as_str() {
            "host.cpu_percent" => match metric.labels.get("collector").map(String::as_str) {
                Some("host-sampler") => row.cpu_host_sampler.push(metric.value),
                Some("sysstat") => row.cpu_sysstat.push(metric.value),
                _ => row.cpu_other.push(metric.value),
            },
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
    for node in nodes {
        for bucket in (first_bucket..=requested_end.timestamp()).step_by(options.bucket as usize) {
            rows.entry((node.clone(), bucket)).or_default();
        }
    }
    println!("run {id}");
    println!("benchmark {} .. {}", start.to_rfc3339(), end.to_rfc3339());
    print_series_coverage(&manifest.collectors);
    println!(
        "bucket {}s; A/M=average/max; HTTP P95=max-of-p95 (not recomputed); -=not observed (see coverage)",
        options.bucket
    );
    println!(
        "{:<10} {:<7} {:>11} {:>9} {:>8} {:>11} {:>11} {:>9} {:>9} {:>7} {:>9} {:>10}",
        "NODE",
        "ELAPSED",
        "CPU A/M%",
        "MEM MiB",
        "LOAD MAX",
        "DISK U%",
        "AWAIT ms",
        "HTTP REQ",
        "P95 ms",
        "ERRORS",
        "DB CALLS",
        "DB TIME ms"
    );
    if rows.is_empty() {
        println!("no timestamped metrics in benchmark interval");
    }
    for ((node, bucket), row) in rows {
        let bucket_offset = bucket - start.timestamp();
        let from = bucket_offset.max(0);
        let to = (bucket_offset + bucket_seconds).min(duration.max(1));
        println!(
            "{:<10} {:<7} {:>11} {:>9} {:>8} {:>11} {:>11} {:>9} {:>9} {:>7} {:>9} {:>10}",
            node,
            format!("{from}-{to}s"),
            avg_max(preferred_cpu(&row)),
            average(&row.memory),
            maximum(&row.load),
            maximum(&row.disk_util),
            maximum(&row.disk_await),
            sum_or_missing(row.http_requests, &row.http_p95, row.http_errors),
            maximum(&row.http_p95),
            count_or_missing(row.http_errors, row.http_requests),
            count_or_missing(row.db_calls, row.db_time),
            count_or_missing(row.db_time, row.db_calls),
        );
    }
    Ok(())
}

fn metric_matches(
    metric: &isuscope::model::Metric,
    options: &SeriesOptions,
    first_bucket: i64,
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
    metric
        .timestamp
        .is_some_and(|at| at.timestamp() >= first_bucket && at <= end)
}

#[derive(Debug, Clone, Copy)]
enum SeriesAggregation {
    Sum,
    Average,
    MaxOfQuantile,
}

impl SeriesAggregation {
    fn name(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Average => "average",
            Self::MaxOfQuantile => "max-of-quantile",
        }
    }
}

fn aggregation_for(metric: &isuscope::model::Metric) -> SeriesAggregation {
    if metric.labels.contains_key("quantile") {
        return SeriesAggregation::MaxOfQuantile;
    }
    if matches!(
        metric.name.as_str(),
        "http.requests"
            | "http.errors"
            | "http.response_bytes"
            | "http.connection_reused_requests"
            | "db.query.calls"
            | "db.query.total_duration"
            | "cpu.sample_count"
    ) {
        SeriesAggregation::Sum
    } else {
        SeriesAggregation::Average
    }
}

fn show_generic_series(
    id: &str,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    options: &SeriesOptions,
    metrics: Vec<isuscope::model::Metric>,
) -> Result<()> {
    type SeriesKey = (String, i64, String, String, String);
    let mut rows = BTreeMap::<SeriesKey, (SeriesAggregation, Vec<f64>)>::new();
    let bucket_seconds = options.bucket as i64;
    for metric in metrics {
        let Some(at) = metric.timestamp else { continue };
        let node = metric
            .labels
            .get("node")
            .cloned()
            .unwrap_or_else(|| "local".into());
        let bucket = at.timestamp().div_euclid(bucket_seconds) * bucket_seconds;
        let labels = metric
            .labels
            .iter()
            .filter(|(key, _)| key.as_str() != "node")
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(",");
        let aggregation = aggregation_for(&metric);
        rows.entry((node, bucket, metric.name, metric.unit, labels))
            .or_insert_with(|| (aggregation, Vec::new()))
            .1
            .push(metric.value);
    }
    println!("run {id}");
    println!("benchmark {} .. {}", start.to_rfc3339(), end.to_rfc3339());
    println!(
        "bucket {}s; quantiles use max-of-quantile and are not recomputed",
        options.bucket
    );
    println!(
        "{:<10} {:<12} {:<34} {:>14} {:<14} {:<16}  LABELS",
        "NODE", "ELAPSED", "METRIC", "VALUE", "UNIT", "AGGREGATION"
    );
    let empty = rows.is_empty();
    let row_count = rows.len();
    for ((node, bucket, name, unit, labels), (aggregation, values)) in
        rows.into_iter().take(options.limit)
    {
        let value = match aggregation {
            SeriesAggregation::Sum => values.iter().sum(),
            SeriesAggregation::Average => values.iter().sum::<f64>() / values.len() as f64,
            SeriesAggregation::MaxOfQuantile => {
                values.iter().copied().reduce(f64::max).unwrap_or_default()
            }
        };
        let from = (bucket - start.timestamp()).max(0);
        let to =
            (bucket - start.timestamp() + bucket_seconds).min((end - start).num_seconds().max(1));
        println!(
            "{:<10} {:<12} {:<34} {:>14.3} {:<14} {:<16}  {}",
            node,
            format!("{from}-{to}s"),
            name,
            value,
            unit,
            aggregation.name(),
            labels
        );
    }
    if empty {
        println!("no matching timestamped metrics");
    } else if row_count > options.limit {
        println!(
            "... truncated {} rows (use --limit to change the cap)",
            row_count - options.limit
        );
    }
    Ok(())
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

fn print_series_coverage(collectors: &[isuscope::model::CollectorResult]) {
    const SERIES_COLLECTORS: [&str; 6] = [
        "host-sampler",
        "sysstat",
        "nginx-log-delta",
        "nginx-series",
        "mysql-log-delta",
        "perf-series",
    ];
    println!("coverage");
    let relevant = collectors
        .iter()
        .filter(|collector| SERIES_COLLECTORS.contains(&collector.name.as_str()))
        .collect::<Vec<_>>();
    if relevant.is_empty() {
        println!("  no standard series collectors recorded");
    }
    for collector in relevant {
        let detail = collector.error.clone().unwrap_or_else(|| {
            collector
                .exit_code
                .map(|code| format!("exit {code}"))
                .unwrap_or_else(|| "-".into())
        });
        println!(
            "  {:<20} {:<12} {:<10} {}",
            collector.name,
            collector.node.as_deref().unwrap_or("local"),
            collector.status,
            detail
        );
    }
}

fn average(values: &[f64]) -> String {
    if values.is_empty() {
        "-".into()
    } else {
        format!("{:.1}", values.iter().sum::<f64>() / values.len() as f64)
    }
}

fn maximum(values: &[f64]) -> String {
    values
        .iter()
        .copied()
        .reduce(f64::max)
        .map(|value| format!("{value:.1}"))
        .unwrap_or_else(|| "-".into())
}

fn avg_max(values: &[f64]) -> String {
    if values.is_empty() {
        "-".into()
    } else {
        format!("{}/{}", average(values), maximum(values))
    }
}

fn count_or_missing(value: f64, evidence: f64) -> String {
    if value == 0.0 && evidence == 0.0 {
        "-".into()
    } else {
        format!("{value:.0}")
    }
}

fn sum_or_missing(value: f64, p95: &[f64], errors: f64) -> String {
    if value == 0.0 && p95.is_empty() && errors == 0.0 {
        "-".into()
    } else {
        format!("{value:.0}")
    }
}

fn show_bottlenecks(config: &LoadedConfig, requested: &str) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    let id = store
        .resolve_id(requested)?
        .with_context(|| format!("run `{requested}` was not found"))?;
    let manifest = store.load(&id)?;
    let report = bottleneck::rank(&store.metrics(&id)?, &manifest.collectors);
    println!("run {id}");
    println!("candidates: one leader per observed category, then category-local severity");
    println!("note: numbers are not a cross-category remediation priority");
    if report.candidates.is_empty() {
        println!("no supported metrics were collected");
    }
    println!(
        "{:<4}  {:<10}  {:<12}  {:<32}  {:<38}  {:<20}",
        "NO.", "CATEGORY", "NODE", "TARGET", "EVIDENCE", "SOURCE"
    );
    for (index, item) in report.candidates.iter().enumerate() {
        println!(
            "{:<4}  {:<10}  {:<12}  {:<32}  {:<38}  {:<20}",
            index + 1,
            item.category,
            item.node,
            item.target,
            item.evidence,
            item.source
        );
        println!("      verify: {}", item.verify_metric);
        println!("      strength: {}", item.strength);
    }
    println!("coverage");
    for coverage in report.coverage {
        println!(
            "  {:<10} {:<11} {}",
            coverage.category,
            if coverage.available {
                "complete"
            } else {
                "unavailable"
            },
            coverage.detail
        );
    }
    Ok(())
}

fn show(config: &LoadedConfig, requested: Option<&str>) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    let Some(requested) = requested else {
        let runs = store.list(20)?;
        if runs.is_empty() {
            println!("no runs");
            print_sqlite_hint(&store, None);
            return Ok(());
        }
        println!(
            "{:<9}  {:<19}  {:<13}  {:<13}  {:<9}  {:>10}  {:<12}",
            "RUN", "STARTED", "COMMIT", "MODE", "RESULT", "SCORE", "ANALYSIS"
        );
        for run in runs {
            print_summary(&run);
        }
        print_sqlite_hint(&store, None);
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
    println!("hypothesis  {}", manifest.hypothesis);
    println!("analysis    {}", manifest.analysis_status.as_str());
    println!("revisions   {}", manifest.analyses.len());
    for analysis in &manifest.analyses {
        println!(
            "  {}  {}  {}",
            analysis.created_at.to_rfc3339(),
            analysis.verdict.as_str(),
            analysis.body.replace('\n', "\n                          ")
        );
    }
    println!("note        {}", manifest.note.as_deref().unwrap_or("-"));
    println!(
        "tags        {}",
        if manifest.tags.is_empty() {
            "-".into()
        } else {
            manifest.tags.join(",")
        }
    );
    println!("state       {}", manifest.state.as_str());
    println!(
        "commit      {}",
        manifest.source.commit_hash.as_deref().unwrap_or("-")
    );
    println!("dirty       {}", manifest.source.dirty);
    println!("state hash  {}", manifest.source.state_sha256);
    println!("isuscope    {}", manifest.tooling.isuscope_version);
    println!("config hash {}", manifest.tooling.config_sha256);
    if let Some(context) = &manifest.codex_context {
        println!("codex file  {}", context.history_path);
        println!("codex input {}", context.input_id);
        println!("codex sess  {}", context.session_id);
        println!("codex hash  {}", context.sha256);
        println!("codex copy  {}", context.snapshot_path);
    }
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
    print_collector_overview(&manifest.collectors);
    println!("enrichments {}", manifest.enrichments.len());
    println!("metrics     {}", manifest.metric_count);
    print_metric_overview(&store.metrics(&id)?);
    println!("fingerprints {}", manifest.fingerprint_count);
    println!("transitions {}", manifest.transition_count);
    println!("logs        {}", manifest.logs.len());
    for log in &manifest.logs {
        println!("  {}", log.id);
        let path = config
            .data_dir
            .join("runs")
            .join(&manifest.id)
            .join("logs")
            .join(format!("{}.zst", log.id));
        println!(
            "    view    zstd -dc -- {}",
            shell_quote(&path.display().to_string())
        );
    }
    println!(
        "path        {}",
        runner::run_path(store.data_dir(), &id).display()
    );
    print_sqlite_hint(&store, Some(&id));
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn print_sqlite_hint(store: &Store, run_id: Option<&str>) {
    let path = store.data_dir().join("isuscope.sqlite3");
    println!("sqlite      {} (shared by all runs)", path.display());
    match run_id {
        Some(run_id) => {
            let run_id = run_id.replace('\'', "''");
            println!(
                "sql hint    sqlite3 {} \"SELECT observed_at,name,value,unit,labels_json FROM metrics WHERE run_id='{}' ORDER BY observed_at;\"",
                shell_display(&path),
                run_id
            );
            println!(
                "context     sqlite3 {} \"SELECT history_path,input_id,session_id,sha256 FROM run_codex_context WHERE run_id='{}';\"",
                shell_display(&path),
                run_id
            );
        }
        None => println!(
            "sql hint    sqlite3 {} \"SELECT id,started_at,score,state,analysis_status,hypothesis FROM runs ORDER BY started_at DESC;\"",
            shell_display(&path)
        ),
    }
    println!(
        "compare     sqlite3 {} \"SELECT substr(run_id,-8) AS run,name,value,unit,labels_json FROM metrics WHERE name='http.request_duration' ORDER BY labels_json,run_id;\"",
        shell_display(&path)
    );
}

fn shell_display(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn print_collector_overview(collectors: &[isuscope::model::CollectorResult]) {
    if collectors.is_empty() {
        return;
    }
    println!("observability");
    for collector in collectors {
        let node = collector.node.as_deref().unwrap_or("local");
        println!(
            "  {:<12} {:<24} {:<12} {}",
            collector.status, collector.name, node, collector.phase
        );
    }
}

fn print_metric_overview(metrics: &[isuscope::model::Metric]) {
    if metrics.is_empty() {
        return;
    }
    let mut series = BTreeMap::<(&str, &str), (usize, f64, f64)>::new();
    for metric in metrics {
        let entry =
            series
                .entry((&metric.name, &metric.unit))
                .or_insert((0, metric.value, metric.value));
        entry.0 += 1;
        entry.1 = entry.1.min(metric.value);
        entry.2 = entry.2.max(metric.value);
    }
    println!("metric series");
    println!(
        "  {:<34} {:>6}  {:>12}  {:>12}  UNIT",
        "NAME", "ROWS", "MIN", "MAX"
    );
    for ((name, unit), (rows, min, max)) in series {
        println!("  {name:<34} {rows:>6}  {min:>12.2}  {max:>12.2}  {unit}");
    }
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
        "{:<9}  {:<19}  {:<13}  {:<13}  {:<9}  {:>10}  {:<12}",
        runner::short_id(&run.id),
        started,
        format!("{commit}{dirty}"),
        run.mode,
        result,
        score,
        run.analysis_status,
    );
    println!(
        "           hypothesis  {}",
        compact_text(&run.hypothesis, 100)
    );
    if let (Some(verdict), Some(body)) = (&run.latest_analysis_verdict, &run.latest_analysis_body) {
        println!(
            "           analysis    {}: {}",
            verdict,
            compact_text(body, 100)
        );
    }
}

fn compact_text(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut truncated = compact.chars().take(max_chars).collect::<String>();
    truncated.push('…');
    truncated
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
}
