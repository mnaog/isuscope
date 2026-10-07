use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use isuscope::{
    brief,
    config::LoadedConfig,
    doctor, enrichment, init,
    metric_semantics::{self, MetricAggregation},
    model::{AnalysisVerdict, RunManifest, RunMode},
    query::{self, DatabaseQueryOptions, HttpQueryOptions, MetricQueryOptions, QueryScope},
    report::{self, RunDiagnostics},
    runner::{self, RunAnnotations},
    shutdown::Shutdown,
    storage::{RunSummary, Store},
};
use std::collections::{BTreeMap, BTreeSet};
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
    /// 変更単位の採否を記録・検索します（deployやmergeは行いません）。
    Change {
        #[command(subcommand)]
        command: ChangeCommand,
    },
    /// プロジェクトへ一度だけ使う設定雛形を生成します。
    Init {
        /// 雛形を書かず、collector設定を標準出力へ出します（他のtoolから取り込む用）。
        #[arg(long = "print", value_parser = ["config"])]
        print: Option<String>,
        /// 生成する設定の`data_dir`。
        #[arg(long, default_value = ".isuscope/data")]
        data_dir: String,
        /// alpとnginx log collectorが読むaccess log。
        #[arg(long, default_value = "/var/log/nginx/access.log")]
        nginx_access_log: String,
        /// slpとMySQL log collectorが読むslow log。
        #[arg(long, default_value = "/var/log/mysql/mysql-slow.log")]
        mysql_slow_log: String,
        /// cgroupから追うsystemd unit。空白区切り。
        #[arg(long, default_value = "")]
        service_units: String,
        /// `doctor`がparserを当てる、保存済みベンチ出力のpath。
        #[arg(long)]
        sample_output: Option<String>,
        /// `[lock]`・`[ssh]`・`[[nodes]]`の例を付けません。自分で追記する場合に使います。
        #[arg(long)]
        no_scaffold: bool,
    },
    /// 変更系操作の共通lockを取ってcommandを実行します。取得済みの子processでは再取得しません。
    Lock {
        /// lock directory。省略時は`[lock] path`を使います。
        #[arg(long)]
        path: Option<PathBuf>,
        /// 実行するcommandと引数（`--`の後に指定）。
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// HTTP route正規化の規則候補を生成します。
    Routes {
        #[command(subcommand)]
        command: RoutesCommand,
    },
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
    /// runの判断材料だけを小さい機械向けJSONで出力します。
    Brief {
        /// `latest`、run ID、一意な短縮ID、または一意なtagを指定します。
        #[arg(default_value = "latest")]
        run: String,
        /// 各性能sectionの上限。
        #[arg(long, default_value_t = 5, value_parser = parse_list_limit)]
        limit: usize,
    },
    /// 保存済みindexへ読み取り専用のSQLを実行します。briefとqueryで足りない問いに使います。
    Sql {
        /// 実行するSELECT。`--schema`と同時には指定できません。
        #[arg(conflicts_with = "schema", required_unless_present = "schema")]
        query: Option<String>,
        /// indexのCREATE文を出力します。
        #[arg(long)]
        schema: bool,
        /// 返す行数の上限（`query`・`series`と同じ既定値）。出力は約12KBでも切ります。
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// 出力形式。
        #[arg(long, value_enum, default_value_t = isuscope::sql::SqlFormat::Json)]
        format: isuscope::sql::SqlFormat,
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
        /// このprefixで始まるmetricを返します（`query`と同じ）。指定時は汎用metric行になります。
        #[arg(long)]
        metric_prefix: Option<String>,
        /// collectorかparserで絞り込みます（`query`と同じ）。
        #[arg(long)]
        source: Option<String>,
        /// `key=value`形式のlabel完全一致。複数回指定できます。
        #[arg(long = "label", value_parser = parse_label_filter)]
        labels: Vec<(String, String)>,
        /// `key=value`形式で、labelの値が`value`を含むものに絞ります。複数回指定できます。
        #[arg(long = "label-contains", value_parser = parse_label_filter)]
        label_contains: Vec<(String, String)>,
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
        /// 返す行数の上限（`query`・`sql`と同じ既定値）。出力は約12KBでも切ります。
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// 保存済みmetricをSQLiteから絞り込み、意味に沿って構造化JSONで返します。
    #[command(after_help = "例:
  isuscope query latest --base BASE_RUN --view http --label route=/api/user/:id --limit 20
  isuscope query latest --base BASE_RUN --view database --window load --group-by sql-shape --label-contains digest=user_items --limit 20
  isuscope query latest --scope series --window load --metric-prefix host. --node app1")]
    Query {
        /// `latest`、run ID、一意な短縮ID、または一意なtagを指定します。
        #[arg(default_value = "latest")]
        run: String,
        /// 同じselectorを適用して比較する基準run。
        #[arg(long)]
        base: Option<String>,
        /// `--base`の比較ですべての値の列を出します。既定は判断に使う列（主要な値の今回と差の割合、
        /// エラー数・検査行数の前後）だけです。
        #[arg(long, requires = "base")]
        all_columns: bool,
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
        /// 比較元run。解決した完全なIDを分析に保存します。
        #[arg(long)]
        base: Option<String>,
        /// 結果の分析本文。skippedでは省略する理由として扱います。
        #[arg(long, required = true)]
        analysis: String,
        /// 同時に採否を記録する変更ID。未作成なら作成します。
        #[arg(long, requires = "decision")]
        change: Option<String>,
        /// `--change`の採否。理由には分析本文を使い、根拠runはこのrunと`--base`です。
        #[arg(long, value_enum, requires = "change")]
        decision: Option<isuscope::changes::DecisionStatus>,
        /// 変更を新しく作るときの説明。省略時はrunの仮説を使います。
        #[arg(long, requires = "change")]
        description: Option<String>,
        /// provisionalのときの再評価条件。
        #[arg(long, requires = "change")]
        revisit: Option<String>,
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
        /// 旧`nginx-series` collectorの引数。route別の集計と時系列はnode上の`alp`へ移ったので、
        /// 渡されたら設定の更新を求めて失敗する。
        #[arg(long, hide = true)]
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

#[derive(Subcommand)]
enum ChangeCommand {
    /// 既存の変更へ採否を追記します。変更は`analyze --change`が作ります。
    Decide {
        id: String,
        #[arg(value_enum)]
        status: isuscope::changes::DecisionStatus,
        #[arg(long = "run", required = true)]
        runs: Vec<String>,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        revisit: Option<String>,
    },
    /// 変更を現在の採否で一覧します。
    List {
        #[arg(long, value_enum)]
        status: Option<isuscope::changes::DecisionStatus>,
        #[arg(long, default_value_t = 20, value_parser = parse_list_limit)]
        limit: usize,
    },
    /// 変更の説明と判断の全履歴を出力します。
    Show { id: String },
}

#[derive(Subcommand)]
enum RoutesCommand {
    /// 動的なIDやkeyが残るrouteから`[[routes]]`候補を作ります。`routes.toml`は変更しません。
    Suggest {
        /// `latest`、run ID、一意な短縮ID、または一意なtagを指定します。
        #[arg(default_value = "latest")]
        run: String,
        /// 候補の書き込み先。省略時は標準出力です。
        #[arg(long)]
        output: Option<PathBuf>,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Commands::Lock { path, command } = &cli.command {
        return match run_lock_command(path.as_deref(), command) {
            Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
            Err(error) => exit_for_error(error),
        };
    }
    // The benchmark lock is taken before the async runtime starts so that marking it held in
    // the environment happens while this process is still single-threaded.
    let _run_lock = match acquire_run_lock(&cli) {
        Ok(lock) => lock,
        Err(error) => return exit_for_error(error),
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return exit_for_error(error.into()),
    };
    match runtime.block_on(real_main(cli)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => exit_for_error(error),
    }
}

fn exit_for_error(error: anyhow::Error) -> ExitCode {
    eprintln!("error: {error:#}");
    if error.downcast_ref::<isuscope::lock::LockBusy>().is_some() {
        ExitCode::from(isuscope::lock::BUSY_EXIT_CODE)
    } else {
        ExitCode::from(2)
    }
}

fn run_lock_command(path: Option<&std::path::Path>, command: &[String]) -> Result<i32> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => {
            let current = env::current_dir().context("cannot determine current directory")?;
            LoadedConfig::discover(&current)?
                .lock_path()
                .context("isuscope lock needs --path or [lock] path in the config")?
        }
    };
    isuscope::lock::run_locked(&path, command)
}

fn acquire_run_lock(cli: &Cli) -> Result<Option<isuscope::lock::OperationLock>> {
    let operation = match cli.command {
        Commands::Run { .. } => "isuscope run",
        Commands::SurveyRun { .. } => "isuscope survey-run",
        _ => return Ok(None),
    };
    let current = env::current_dir().context("cannot determine current directory")?;
    let Some(path) = LoadedConfig::discover(&current)?.lock_path() else {
        return Ok(None);
    };
    let lock = isuscope::lock::OperationLock::acquire(&path, operation)?;
    if lock.is_some() {
        // SAFETY: no other threads exist yet; the tokio runtime is built afterwards.
        unsafe { env::set_var(isuscope::lock::HELD_ENV, "1") };
    }
    Ok(lock)
}

async fn real_main(cli: Cli) -> Result<bool> {
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
        series_only,
    } = &cli.command
    {
        if *series_only {
            anyhow::bail!(
                "the nginx-series collector was replaced by the node-side `alp` collector; \
                 remove nginx-series from .isuscope/config.toml and take the alp, nginx-log-raw \
                 and user-transition collectors from `isuscope init --print`"
            );
        }
        isuscope::transition::emit(isuscope::transition::TransitionOptions {
            run_dir,
            prefix,
            rules: rules.as_deref(),
            time_field,
            session_field,
            method_field,
            uri_field,
        })?;
        return Ok(true);
    }
    let current = env::current_dir().context("cannot determine current directory")?;
    if let Commands::Init {
        print,
        data_dir,
        nginx_access_log,
        mysql_slow_log,
        service_units,
        sample_output,
        no_scaffold,
    } = &cli.command
    {
        let options = isuscope::init::ConfigOptions {
            data_dir: data_dir.clone(),
            nginx_access_log: nginx_access_log.clone(),
            mysql_slow_log: mysql_slow_log.clone(),
            service_units: service_units
                .split_whitespace()
                .map(ToOwned::to_owned)
                .collect(),
            sample_output: sample_output.clone(),
            scaffold: !no_scaffold,
        };
        match print.as_deref() {
            // The starter template renders the same collectors and adds its own SSH and nodes.
            Some("config") => print!("{}", isuscope::init::render_config(&options)),
            _ => init::scaffold_with(&current, &options)?,
        }
        return Ok(true);
    }
    let config = LoadedConfig::discover(&current)?;
    match cli.command {
        Commands::Init { .. } | Commands::Lock { .. } => unreachable!(),
        Commands::Routes {
            command: RoutesCommand::Suggest { run, output },
        } => {
            let (content, rules) = isuscope::project_tools::suggest_routes(&config, &run)?;
            match output {
                Some(path) => {
                    isuscope::project_tools::write_output(&path, &content)?;
                    eprintln!("route suggestions: {} ({rules} rules)", path.display());
                }
                None => print!("{content}"),
            }
            Ok(true)
        }
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
        Commands::Brief { run, limit } => {
            show_brief(&config, &run, limit)?;
            Ok(true)
        }
        Commands::Sql {
            query,
            schema,
            limit,
            format,
        } => {
            if schema {
                print!("{}", isuscope::sql::schema(&config)?);
                return Ok(true);
            }
            let query = query.context("a SELECT statement or --schema is required")?;
            let output = isuscope::sql::query(&config, &query, limit)?;
            match format {
                isuscope::sql::SqlFormat::Json => write_sql_json(&output)?,
                isuscope::sql::SqlFormat::Tsv => {
                    isuscope::sql::write_tsv(&output, std::io::stdout().lock())?
                }
            }
            Ok(true)
        }
        Commands::Series {
            run,
            metrics,
            metric_prefix,
            source,
            node,
            labels,
            label_contains,
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
                    metric_prefix,
                    source,
                    node,
                    labels,
                    label_contains,
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
            all_columns,
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
                all_columns,
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
            base,
            analysis: body,
            change,
            decision,
            description,
            revisit,
        } => {
            let skipped = matches!(verdict, VerdictArg::Skipped);
            if body.trim().is_empty() {
                anyhow::bail!(if skipped {
                    "the skipped verdict requires a non-empty reason"
                } else {
                    "analysis must not be empty"
                });
            }
            let mut store = Store::open(&config.data_dir)?;
            let id = store.require_id(&run, "run")?;
            let base = base
                .map(|requested| store.require_id(&requested, "base run"))
                .transpose()?;
            // Validate the decision before appending, so a rejected decision leaves no analysis.
            let decision = match (change, decision) {
                (Some(change), Some(status)) => Some(store.prepare_change_decision(
                    &change,
                    &id,
                    status,
                    description,
                    revisit,
                )?),
                _ => None,
            };
            let manifest =
                store.append_analysis_with_base(&id, verdict.into(), body.clone(), base.clone())?;
            let latest = manifest
                .analyses
                .last()
                .context("analysis was not appended")?;
            println!("run       {}", runner::short_id(&manifest.id));
            println!("verdict   {}", latest.verdict.as_str());
            println!("analysis  {}", manifest.analysis_status.as_str());
            println!("revisions {}", manifest.analyses.len());
            if let Some(prepared) = decision {
                let mut runs = vec![id.clone()];
                runs.extend(base);
                let recorded = store.record_change_decision(prepared, body, runs)?;
                println!("change    {}", recorded.change_id);
                println!("decision  {}", recorded.status.as_str());
            }
            Ok(true)
        }
        Commands::Change { command } => {
            let mut store = Store::open(&config.data_dir)?;
            match command {
                ChangeCommand::Decide {
                    id,
                    status,
                    runs,
                    reason,
                    revisit,
                } => write_stdout_json(&isuscope::changes::DecisionView::from(
                    store.decide_change(&id, status, reason, revisit, runs)?,
                ))?,
                ChangeCommand::List { status, limit } => {
                    let mut changes = store
                        .list_changes(status, None, 100_000)?
                        .into_iter()
                        .map(isuscope::changes::ChangeSummaryView::from)
                        .collect::<Vec<_>>();
                    let total_count = changes.len();
                    changes.truncate(limit);
                    write_capped_json(
                        &serde_json::json!({
                            "schema_version": isuscope::model::OUTPUT_SCHEMA_VERSION,
                            "changes": Listed::new(changes, total_count),
                        }),
                        &CHANGE_LIST_CAP,
                    )?
                }
                ChangeCommand::Show { id } => write_stdout_json(
                    &isuscope::changes::ChangeHistoryView::from(store.change_history(&id)?),
                )?,
            }
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
    /// SQL別の5秒bucket（slp以前の自前解析）の合計。
    db_calls: f64,
    db_time: f64,
    /// DB全体の5秒bucket（slp collectorの`db.calls`・`db.duration`）。あればこちらを使う。
    db_total_calls: Option<f64>,
    db_total_time: Option<f64>,
}

impl BucketRow {
    /// DB全体の値があればそれを、無ければSQL別の値の合計を返す。両方を足すと二重に数える。
    fn database(&self) -> (Option<f64>, Option<f64>) {
        if self.db_total_calls.is_some() || self.db_total_time.is_some() {
            return (
                Some(self.db_total_calls.unwrap_or_default()),
                Some(self.db_total_time.unwrap_or_default()),
            );
        }
        (
            observed_sum(self.db_calls, self.db_time != 0.0),
            observed_sum(self.db_time, self.db_calls != 0.0),
        )
    }
}

#[derive(Debug)]
struct SeriesOptions {
    metrics: Vec<String>,
    metric_prefix: Option<String>,
    source: Option<String>,
    node: Option<String>,
    labels: Vec<(String, String)>,
    label_contains: Vec<(String, String)>,
    from: u64,
    to: Option<u64>,
    window: SeriesWindowArg,
    bucket: u64,
    limit: usize,
}

#[derive(serde::Serialize)]
struct SeriesOutput {
    schema_version: u32,
    #[serde(serialize_with = "isuscope::model::serialize_short_run")]
    run: String,
    /// 区間の名前（`whole`・`initialize`・`load`）。ほかの出力の`window`と同じ。
    window: &'static str,
    /// 区間の実際の時刻とbucket。
    range: SeriesRange,
    /// `from_seconds`の起点。区間がベンチ全体（`whole`）なら`range`と同じなので出さない。
    #[serde(skip_serializing_if = "Option::is_none")]
    benchmark: Option<SeriesInterval>,
    /// 完了しなかったcollectorだけ。完了したものは並べない。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    coverage: Vec<SeriesCoverage>,
    #[serde(flatten)]
    data: SeriesData,
    warnings: Vec<String>,
}

#[derive(serde::Serialize)]
struct SeriesInterval {
    started_at: String,
    finished_at: String,
}

#[derive(serde::Serialize)]
struct SeriesRange {
    started_at: String,
    finished_at: String,
    from_seconds: i64,
    to_seconds: i64,
    bucket_seconds: u64,
    /// 5秒bucketで集計した値（HTTP、DB、perf、client）の区間の端。`exact`は端がbucketの区切りか、
    /// node上で振り分けた境界（ベンチの始まりと終わり）に一致している。`approximate`は端をまたぐ
    /// bucketがあり、始まり側のbucketは含めず、終わり側のbucketは含めている。
    edges: &'static str,
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
        /// metricごとの単位と時系列の集計方法。行では繰り返さない。
        metrics: BTreeMap<String, SeriesSemantics>,
        total_count: usize,
        truncated: bool,
        rows: Vec<MetricSeriesRow>,
    },
}

#[derive(serde::Serialize, Clone, PartialEq)]
struct SeriesSemantics {
    unit: String,
    aggregation: MetricAggregation,
}

#[derive(serde::Serialize)]
struct OverviewSeriesRow {
    node: String,
    from_seconds: i64,
    to_seconds: i64,
    cpu_busy_avg_percent: Option<f64>,
    cpu_busy_max_percent: Option<f64>,
    memory_used_avg_mib: Option<f64>,
    load1_max: Option<f64>,
    disk_util_max_percent: Option<f64>,
    disk_await_max_ms: Option<f64>,
    http_requests: Option<f64>,
    http_p95_max_ms: Option<f64>,
    http_errors: Option<f64>,
    db_calls: Option<f64>,
    db_duration_total_ms: Option<f64>,
}

#[derive(serde::Serialize)]
struct MetricSeriesRow {
    node: String,
    from_seconds: i64,
    to_seconds: i64,
    metric: String,
    value: f64,
    #[serde(skip)]
    unit: String,
    #[serde(skip)]
    aggregation: MetricAggregation,
    /// 同じmetric名で単位か集計方法が行ごとに違うときだけ、行に出す。
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    semantics: Option<SeriesSemantics>,
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

#[allow(clippy::too_many_arguments)]
fn show_query(
    config: &LoadedConfig,
    requested: &str,
    base_requested: Option<&str>,
    all_columns: bool,
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
    let id = store.require_id(requested, "run")?;
    let base_id = base_requested
        .map(|requested| store.require_id(requested, "base run"))
        .transpose()?;
    let query_limit = base_id.as_ref().map_or(limit, |_| usize::MAX);
    // database viewの区間はnode上で集計した行のlabelなので、`--scope run`のまま選べる。
    if !matches!(scope, QueryScopeArg::Series)
        && window != SeriesWindowArg::Whole
        && !matches!(view, QueryViewArg::Database)
    {
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
            if needs_cpu_sample_counts(&metrics, metric_prefix.as_deref(), scope)
                && !candidate_metrics
                    .iter()
                    .any(|metric| metric.name == "cpu.sample_count")
            {
                candidate_metrics.extend(store.query_metrics(
                    &id,
                    &["cpu.sample_count".into()],
                    None,
                    Some(true),
                )?);
            }
            if matches!(scope, QueryScopeArg::Series) {
                let manifest = store.load(&id)?;
                let (start, end) = named_window(&manifest, window)?;
                candidate_metrics.retain(|metric| {
                    metric
                        .timestamp
                        .is_some_and(|timestamp| isuscope::model::in_window(timestamp, start, end))
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
            let mut candidate = query::metric_query(id.clone(), candidate_metrics, options.clone());
            if candidate.total_count == 0
                && let Some(hint) = empty_metric_hint(
                    &store,
                    &id,
                    &options.metrics,
                    options.metric_prefix.as_deref(),
                    matches!(scope, QueryScopeArg::Series),
                )?
            {
                candidate.warnings.push(hint);
            }
            if let Some(base_id) = base_id {
                let mut base_metrics = store.query_metrics(
                    &base_id,
                    &options.metrics,
                    options.metric_prefix.as_deref(),
                    Some(options.scope == QueryScope::Series),
                )?;
                if needs_cpu_sample_counts(
                    &options.metrics,
                    options.metric_prefix.as_deref(),
                    scope,
                ) && !base_metrics
                    .iter()
                    .any(|metric| metric.name == "cpu.sample_count")
                {
                    base_metrics.extend(store.query_metrics(
                        &base_id,
                        &["cpu.sample_count".into()],
                        None,
                        Some(true),
                    )?);
                }
                if options.scope == QueryScope::Series {
                    let manifest = store.load(&base_id)?;
                    let (start, end) = named_window(&manifest, window)?;
                    base_metrics.retain(|metric| {
                        metric.timestamp.is_some_and(|timestamp| {
                            isuscope::model::in_window(timestamp, start, end)
                        })
                    });
                }
                let base = query::metric_query(base_id, base_metrics, options);
                write_capped_json(
                    &columns(
                        query::metric_query_diff(base, candidate, limit),
                        all_columns,
                    ),
                    &QUERY_CAP,
                )?;
            } else {
                write_rows_json(&candidate, &["metric", "unit", "aggregation"])?;
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
            let candidate_manifest = store.load(&id)?;
            let mut labels = labels;
            if window != SeriesWindowArg::Whole {
                if !isuscope::report::supports_database_windows(
                    &candidate_manifest,
                    &candidate_metrics,
                ) {
                    bail!(
                        "run {} has no per-window database rows (it predates the windowed slp collector); use --window whole",
                        runner::short_id(&id)
                    );
                }
                refuse_unsplit_database_window(&id, &candidate_metrics, window)?;
                labels.push(("window".into(), window.as_str().into()));
            }
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
                let base_manifest = store.load(&base_id)?;
                if window != SeriesWindowArg::Whole
                    && !isuscope::report::supports_database_windows(&base_manifest, &base_metrics)
                {
                    bail!(
                        "base run {} has no per-window database rows; compare with --window whole",
                        runner::short_id(&base_id)
                    );
                }
                if window != SeriesWindowArg::Whole {
                    refuse_unsplit_database_window(&base_id, &base_metrics, window)?;
                }
                let base = query::database_query(base_id, base_metrics, options);
                write_capped_json(
                    &columns(
                        query::database_query_diff(base, candidate, limit),
                        all_columns,
                    ),
                    &QUERY_CAP,
                )?;
            } else {
                write_rows_json(&candidate, &["node", "engine", "source", "window"])?;
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
                write_capped_json(
                    &columns(query::http_query_diff(base, candidate, limit), all_columns),
                    &QUERY_CAP,
                )?;
            } else {
                write_rows_json(&candidate, &["node", "method"])?;
            }
        }
    }
    Ok(())
}

fn needs_cpu_sample_counts(
    requested: &[String],
    prefix: Option<&str>,
    scope: QueryScopeArg,
) -> bool {
    matches!(scope, QueryScopeArg::Series)
        && (requested.is_empty() || requested.iter().any(|name| name == "cpu.sample_percent"))
        && prefix.is_none_or(|prefix| "cpu.sample_percent".starts_with(prefix))
}

fn show_series(config: &LoadedConfig, requested: &str, options: SeriesOptions) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    let id = store.require_id(requested, "run")?;
    let manifest = store.load(&id)?;
    // 境界はbucketの先頭と同じマイクロ秒に揃える（ナノ秒を持った古いrunでも最初のbucketを落とさない）。
    let start =
        isuscope::model::to_micros(manifest.benchmark.started_at.unwrap_or(manifest.started_at));
    let end = isuscope::model::to_micros(
        manifest
            .benchmark
            .finished_at
            .or(manifest.finished_at)
            .unwrap_or(start),
    );
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
                .with_context(|| missing_window(&id, "start", options.window))?,
            manifest
                .benchmark
                .initialize_finished_at
                .with_context(|| missing_window(&id, "end", options.window))?,
        ),
        SeriesWindowArg::Load => (
            manifest
                .benchmark
                .initialize_finished_at
                .with_context(|| missing_window(&id, "end", options.window))?,
            end,
        ),
    };
    let (requested_start, requested_end) = (
        isuscope::model::to_micros(requested_start),
        isuscope::model::to_micros(requested_end),
    );
    let bucket_seconds = options.bucket as i64;
    let duration = (end - start).num_seconds().max(0);
    let metrics = store.metrics(&id)?;
    let edges = series_edges(&manifest, &metrics, requested_start, requested_end);
    let metrics = metrics
        .into_iter()
        .filter(|metric| metric_matches(metric, &options, requested_start, requested_end))
        .collect::<Vec<_>>();
    if !options.metrics.is_empty() || options.metric_prefix.is_some() {
        let data = generic_series_data(start, requested_start, requested_end, &options, metrics);
        let empty = matches!(&data, SeriesData::Metrics { total_count: 0, .. });
        let mut output = series_output(
            id.clone(),
            start,
            end,
            requested_start,
            requested_end,
            &options,
            edges,
            series_coverage(&manifest.collectors),
            data,
        );
        if empty
            && let Some(hint) = empty_metric_hint(
                &store,
                &id,
                &options.metrics,
                options.metric_prefix.as_deref(),
                true,
            )?
        {
            output.warnings.push(hint);
        }
        write_capped_json(&output, &SERIES_CAP)?;
        return Ok(());
    }
    let mut rows = BTreeMap::<(String, i64), BucketRow>::new();
    let mut nodes = BTreeSet::new();
    for metric in metrics {
        let Some(at) = metric.timestamp else { continue };
        let Some(bucket) = bucket_index(at, requested_start, bucket_seconds) else {
            continue;
        };
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
            "db.calls" => *row.db_total_calls.get_or_insert(0.0) += metric.value,
            "db.duration" => *row.db_total_time.get_or_insert(0.0) += metric.value,
            _ => {}
        }
    }
    let window_duration = (requested_end - requested_start).num_seconds().max(0);
    for node in nodes {
        for bucket in 0..=window_duration.div_euclid(bucket_seconds) {
            rows.entry((node.clone(), bucket)).or_default();
        }
    }
    let mut rows = rows
        .into_iter()
        .map(|((node, bucket), row)| {
            let bucket_offset = (requested_start - start).num_seconds() + bucket * bucket_seconds;
            let from = bucket_offset.max(0);
            let to = (bucket_offset + bucket_seconds).min(duration.max(1));
            OverviewSeriesRow {
                node,
                from_seconds: from,
                to_seconds: to,
                cpu_busy_avg_percent: round3(average_value(preferred_cpu(&row))),
                cpu_busy_max_percent: round3(maximum_value(preferred_cpu(&row))),
                memory_used_avg_mib: round3(average_value(&row.memory)),
                load1_max: round3(maximum_value(&row.load)),
                disk_util_max_percent: round3(maximum_value(&row.disk_util)),
                disk_await_max_ms: round3(maximum_value(&row.disk_await)),
                http_requests: round3(observed_sum(
                    row.http_requests,
                    !row.http_p95.is_empty() || row.http_errors != 0.0,
                )),
                http_p95_max_ms: round3(maximum_value(&row.http_p95)),
                http_errors: round3(observed_sum(row.http_errors, row.http_requests != 0.0)),
                db_calls: round3(row.database().0),
                db_duration_total_ms: round3(row.database().1),
            }
        })
        .collect::<Vec<_>>();
    let total_count = rows.len();
    rows.truncate(options.limit);
    write_capped_json(
        &series_output(
            id,
            start,
            end,
            requested_start,
            requested_end,
            &options,
            edges,
            series_coverage(&manifest.collectors),
            SeriesData::Overview {
                total_count,
                truncated: total_count > options.limit,
                rows,
            },
        ),
        &SERIES_CAP,
    )?;
    Ok(())
}

/// `--all-columns`ならすべての値の列を出す。既定は[`query::DiffColumns::Compact`]。
fn columns<T>(diff: query::QueryDiffOutput<T>, all: bool) -> query::QueryDiffOutput<T> {
    if all { diff.with_all_columns() } else { diff }
}

/// 選んだmetricが1行も無いときの理由と次の一手。もう一方の形（run集約か時系列か）にあれば
/// そちらのコマンドを、絞り込みで消えたならそう伝え、run自体に無ければ名前の一覧を引くSQLを示す。
fn empty_metric_hint(
    store: &Store,
    id: &str,
    metrics: &[String],
    prefix: Option<&str>,
    timed: bool,
) -> Result<Option<String>> {
    let selector = match (metrics, prefix) {
        ([], None) => return Ok(None),
        ([], Some(prefix)) => format!("--metric-prefix {prefix}"),
        (names, _) => names
            .iter()
            .map(|name| format!("--metric {name}"))
            .collect::<Vec<_>>()
            .join(" "),
    };
    let short = runner::short_id(id);
    if !store
        .query_metrics(id, metrics, prefix, Some(timed))?
        .is_empty()
    {
        return Ok(Some(format!(
            "`{selector}` exists in run {short}, but no row matched the other filters (--node, --source, --label, --label-contains, --window)"
        )));
    }
    if !store
        .query_metrics(id, metrics, prefix, Some(!timed))?
        .is_empty()
    {
        return Ok(Some(if timed {
            format!(
                "`{selector}` has only run totals in run {short}, no time series; see `isuscope query {short} {selector}`"
            )
        } else {
            format!(
                "`{selector}` has only a time series in run {short}; see `isuscope series {short} {selector}`"
            )
        }));
    }
    Ok(Some(format!(
        "no metric matched `{selector}` in run {short}; list names with `isuscope sql \"SELECT DISTINCT name FROM metrics WHERE run_id LIKE '%{short}' ORDER BY name\"`"
    )))
}

/// initializeの始まりか終わりを記録していないrunで、その区間を選んだとき。どのrunか、何が無いか、
/// どうすればよいかを1文で伝える（`query`のDB区間の拒否と同じ形）。
fn missing_window(id: &str, edge: &str, window: SeriesWindowArg) -> String {
    format!(
        "run {} did not record the {edge} of initialize, so --window {} is unavailable; use --window whole",
        runner::short_id(id),
        window.as_str()
    )
}

/// slpはinitializeの終わりが分かったrunだけをinitializeとloadに分け、分からないrunは全体を
/// `whole`にまとめる。後者で`load`を選ぶと、SQLが無かったかのように0件が返ってしまう。
/// （loadの0件そのものは、負荷区間のSQLを無くせたrunで起こり得るので、行は`initialize`にある。）
fn refuse_unsplit_database_window(
    id: &str,
    metrics: &[isuscope::model::Metric],
    window: SeriesWindowArg,
) -> Result<()> {
    let has = |name: &str| {
        metrics
            .iter()
            .any(|metric| metric.labels.get("window").map(String::as_str) == Some(name))
    };
    if !has(window.as_str()) && has("whole") {
        bail!(
            "run {} did not record the end of initialize, so its database rows are not split; use --window whole",
            runner::short_id(id)
        );
    }
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
                .with_context(|| missing_window(&manifest.id, "start", window))?,
            manifest
                .benchmark
                .initialize_finished_at
                .with_context(|| missing_window(&manifest.id, "end", window))?,
        ),
        SeriesWindowArg::Load => (
            manifest
                .benchmark
                .initialize_finished_at
                .with_context(|| missing_window(&manifest.id, "end", window))?,
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
    edges: &'static str,
    coverage: Vec<SeriesCoverage>,
    data: SeriesData,
) -> SeriesOutput {
    SeriesOutput {
        schema_version: isuscope::model::OUTPUT_SCHEMA_VERSION,
        run: run_id,
        window: options.window.as_str(),
        benchmark: ((window_start, window_end) != (benchmark_start, benchmark_end)).then(|| {
            SeriesInterval {
                started_at: isuscope::model::display_time(benchmark_start),
                finished_at: isuscope::model::display_time(benchmark_end),
            }
        }),
        range: SeriesRange {
            started_at: isuscope::model::display_time(window_start),
            finished_at: isuscope::model::display_time(window_end),
            from_seconds: (window_start - benchmark_start).num_seconds(),
            to_seconds: (window_end - benchmark_start).num_seconds(),
            bucket_seconds: options.bucket,
            edges,
        },
        coverage,
        data,
        warnings: Vec::new(),
    }
}

/// 区間の端が5秒bucketで正確に切れているか（[`SeriesRange::edges`]）。端がbucketの区切りか、
/// node上で行を振り分けた境界（ベンチの始まりと終わり）なら正確。bucketの区切りは保存された
/// bucketから読む（負荷の始まりに揃える前のrunは、epochの5の倍数で区切っている）。
/// 区間の始まりから数えたbucketの番号。差をマイクロ秒で取ってから割る（それぞれを整数秒へ
/// 切ってから引くと、始まりが小数秒のとき境界の近くの値が1つ後ろのbucketへずれる）。
fn bucket_index(
    at: chrono::DateTime<chrono::Utc>,
    start: chrono::DateTime<chrono::Utc>,
    bucket_seconds: i64,
) -> Option<i64> {
    Some(
        (at - start)
            .num_microseconds()?
            .div_euclid(bucket_seconds * 1_000_000),
    )
}

fn series_edges(
    manifest: &isuscope::model::RunManifest,
    metrics: &[isuscope::model::Metric],
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> &'static str {
    const BUCKETED: [&str; 4] = [
        "http.requests",
        "db.calls",
        "cpu.sample_count",
        "client.connections_opened",
    ];
    let origin = manifest.benchmark.bucket_origin();
    let aligned = |at: chrono::DateTime<chrono::Utc>, origin: chrono::DateTime<chrono::Utc>| {
        (at - origin)
            .num_microseconds()
            .is_some_and(|offset| offset.rem_euclid(5_000_000) == 0)
    };
    // ベンチの始まりで切り詰めたbucketは区切りに乗らないので、判定に使わない。
    let mut stored = metrics
        .iter()
        .filter(|metric| BUCKETED.contains(&metric.name.as_str()))
        .filter_map(|metric| metric.timestamp)
        .filter(|at| {
            Some(*at)
                != manifest
                    .benchmark
                    .started_at
                    .map(isuscope::model::to_micros)
        })
        .peekable();
    let origin = match origin {
        Some(origin) if stored.peek().is_none() || stored.all(|at| aligned(at, origin)) => origin,
        _ => chrono::DateTime::UNIX_EPOCH,
    };
    let exact = |at: chrono::DateTime<chrono::Utc>| {
        Some(at)
            == manifest
                .benchmark
                .started_at
                .map(isuscope::model::to_micros)
            || Some(at)
                == manifest
                    .benchmark
                    .finished_at
                    .map(isuscope::model::to_micros)
            || aligned(at, origin)
    };
    if exact(start) && exact(end) {
        "exact"
    } else {
        "approximate"
    }
}

fn metric_matches(
    metric: &isuscope::model::Metric,
    options: &SeriesOptions,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) -> bool {
    let dependency = metric.name == "cpu.sample_count"
        && options
            .metrics
            .iter()
            .any(|name| name == "cpu.sample_percent");
    if !dependency && !options.metrics.is_empty() && !options.metrics.contains(&metric.name) {
        return false;
    }
    if !dependency
        && let Some(prefix) = &options.metric_prefix
        && !metric.name.starts_with(prefix)
    {
        return false;
    }
    // `query`と同じく、sourceはcollectorかparserの名前。
    if let Some(source) = &options.source
        && metric.labels.get("collector") != Some(source)
        && metric.labels.get("isuscope.parser") != Some(source)
    {
        return false;
    }
    if let Some(node) = &options.node
        && metric.labels.get("node") != Some(node)
    {
        return false;
    }
    if !dependency
        && options
            .labels
            .iter()
            .any(|(key, value)| metric.labels.get(key) != Some(value))
    {
        return false;
    }
    if !dependency
        && options.label_contains.iter().any(|(key, needle)| {
            metric
                .labels
                .get(key)
                .is_none_or(|value| !value.contains(needle))
        })
    {
        return false;
    }
    metric
        .timestamp
        .is_some_and(|at| isuscope::model::in_window(at, start, end))
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
    let mut buckets = BTreeMap::<i64, Vec<isuscope::model::Metric>>::new();
    for metric in metrics {
        if let Some(at) = metric.timestamp
            && isuscope::model::in_window(at, window_start, end)
            && let Some(bucket) = bucket_index(at, window_start, bucket_seconds)
        {
            buckets.entry(bucket).or_default().push(metric);
        }
    }
    let selection = query::MetricQueryOptions {
        scope: query::QueryScope::Series,
        window: None,
        metrics: options.metrics.clone(),
        metric_prefix: options.metric_prefix.clone(),
        node: options.node.clone(),
        source: options.source.clone(),
        labels: options.labels.clone(),
        label_contains: options.label_contains.clone(),
        group_by: Vec::new(),
        limit: usize::MAX,
    };
    let metrics = buckets
        .into_values()
        .flat_map(|metrics| query::selected_metrics(metrics, &selection));
    for metric in metrics {
        let Some(at) = metric.timestamp else { continue };
        let node = metric
            .labels
            .get("node")
            .cloned()
            .unwrap_or_else(|| "local".into());
        let Some(bucket) = bucket_index(at, window_start, bucket_seconds) else {
            continue;
        };
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
    let mut rows = rows
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
                    value: query::round_to(value, 3),
                    unit,
                    aggregation,
                    semantics: None,
                    labels,
                }
            },
        )
        .collect::<Vec<_>>();
    let mut metrics = BTreeMap::<String, Option<SeriesSemantics>>::new();
    for row in &rows {
        let semantics = SeriesSemantics {
            unit: row.unit.clone(),
            aggregation: row.aggregation,
        };
        metrics
            .entry(row.metric.clone())
            .and_modify(|known| {
                if known.as_ref() != Some(&semantics) {
                    *known = None;
                }
            })
            .or_insert(Some(semantics));
    }
    for row in &mut rows {
        if metrics.get(&row.metric).is_some_and(Option::is_none) {
            row.semantics = Some(SeriesSemantics {
                unit: row.unit.clone(),
                aggregation: row.aggregation,
            });
        }
    }
    SeriesData::Metrics {
        metrics: metrics
            .into_iter()
            .filter_map(|(name, semantics)| Some((name, semantics?)))
            .collect(),
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
    const SERIES_COLLECTORS: [&str; 9] = [
        "host-sampler",
        "sysstat",
        "service-sampler",
        "nginx-log-delta",
        "alp",
        "nginx-series",
        "mysql-log-delta",
        "slp",
        "perf-series",
    ];
    collectors
        .iter()
        .filter(|collector| SERIES_COLLECTORS.contains(&collector.name.as_str()))
        .filter(|collector| collector.status != "complete")
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

fn round3(value: Option<f64>) -> Option<f64> {
    value.map(|value| query::round_to(value, 3))
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
    runs: Listed<RunSummary>,
}

/// `--limit`で切る一覧。briefの各欄やqueryと同じく、全件数と切ったかどうかを持つ。
#[derive(serde::Serialize)]
struct Listed<T> {
    total_count: usize,
    truncated: bool,
    items: Vec<T>,
}

impl<T> Listed<T> {
    fn new(items: Vec<T>, total_count: usize) -> Self {
        Self {
            truncated: total_count > items.len(),
            total_count,
            items,
        }
    }
}

fn list_runs(config: &LoadedConfig, limit: usize) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    write_capped_json(
        &RunListOutput {
            schema_version: isuscope::model::OUTPUT_SCHEMA_VERSION,
            runs: Listed::new(store.list(limit)?, store.run_count()?),
        },
        &LIST_CAP,
    )?;
    Ok(())
}

/// 機械向けの出力。AIのtool出力には上限があり、超えると真ん中から削られるので字下げを付けない。
/// 人が読むときは`jq`へ通す。
fn write_stdout_json(value: &impl serde::Serialize) -> Result<()> {
    write_json(serde_json::to_value(value)?, None)
}

/// 行を返す出力。約[`OUTPUT_BUDGET_BYTES`]を超えるなら`cap`の行を末尾から減らす。
fn write_capped_json(value: &impl serde::Serialize, cap: &RowCap) -> Result<()> {
    write_json(serde_json::to_value(value)?, Some(cap))
}

fn write_json(mut value: serde_json::Value, cap: Option<&RowCap>) -> Result<()> {
    round_numbers(&mut value);
    tabulate(&mut value);
    if let Some(cap) = cap {
        fit_rows(&mut value, cap)?;
    }
    print_json(&value)
}

/// `sql`は保存値をそのまま返す。丸めも表への組み替えもせず（行は最初から値の並び）、大きさだけ抑える。
fn write_sql_json(output: &isuscope::sql::SqlOutput) -> Result<()> {
    let mut value = serde_json::to_value(output)?;
    fit_rows(&mut value, &SQL_CAP)?;
    print_json(&value)
}

fn print_json(value: &serde_json::Value) -> Result<()> {
    // どの出力にも`warnings`を置く（無ければ空）。有無で形が変わらないように。
    let mut value = value.clone();
    if let Some(fields) = value.as_object_mut() {
        let warnings = fields
            .shift_remove("warnings")
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        fields.insert("warnings".into(), warnings);
    }
    serde_json::to_writer(std::io::stdout().lock(), &value)?;
    println!();
    Ok(())
}

/// 行を識別する値のうち全行で同じもの（1台のDBを見るときのnodeなど）を`common`へ1回だけ出し、
/// 各行では繰り返さない。metric viewでは、全行で同じlabelも`common.labels`へまとめる。
fn write_rows_json(value: &impl serde::Serialize, identity: &[&str]) -> Result<()> {
    let mut value = serde_json::to_value(value)?;
    hoist_common(&mut value, identity);
    write_json(value, Some(&QUERY_CAP))
}

fn hoist_common(value: &mut serde_json::Value, identity: &[&str]) {
    // briefの表は`items`に行を持つ。
    let field = if value.get("rows").is_some() {
        "rows"
    } else {
        "items"
    };
    let Some(rows) = value.get(field).and_then(serde_json::Value::as_array) else {
        return;
    };
    if rows.len() < 2 {
        return;
    }
    let shared = |field: &str| {
        let first = rows[0].get(field)?;
        rows.iter()
            .all(|row| row.get(field) == Some(first))
            .then(|| first.clone())
    };
    let mut common = serde_json::Map::new();
    for field in identity {
        if let Some(shared) = shared(field) {
            common.insert((*field).into(), shared);
        }
    }
    let mut labels = serde_json::Map::new();
    if let Some(first) = rows[0].get("labels").and_then(serde_json::Value::as_object) {
        for (name, label) in first {
            if rows
                .iter()
                .all(|row| row.get("labels").and_then(|labels| labels.get(name)) == Some(label))
            {
                labels.insert(name.clone(), label.clone());
            }
        }
    }
    if common.is_empty() && labels.is_empty() {
        return;
    }
    if let Some(rows) = value
        .get_mut(field)
        .and_then(serde_json::Value::as_array_mut)
    {
        for row in rows.iter_mut().filter_map(serde_json::Value::as_object_mut) {
            // `preserve_order`の`remove`は末尾の列を空いた位置へ移すので、順序を保つ`shift_remove`を使う。
            for field in common.keys() {
                row.shift_remove(field);
            }
            if let Some(row_labels) = row
                .get_mut("labels")
                .and_then(serde_json::Value::as_object_mut)
            {
                row_labels.retain(|name, _| !labels.contains_key(name));
            }
            if row
                .get("labels")
                .and_then(serde_json::Value::as_object)
                .is_some_and(serde_json::Map::is_empty)
            {
                row.shift_remove("labels");
            }
        }
    }
    if !labels.is_empty() {
        common.insert("labels".into(), serde_json::Value::Object(labels));
    }
    if let Some(fields) = value.as_object_mut() {
        fields.insert("common".into(), serde_json::Value::Object(common));
    }
}

/// 行を持つ出力の大きさの上限。AIのtool出力は上限（Codexで約4500 tokens）を超えると真ん中から
/// 削られ、どこが欠けたか分からなくなる。超えるときは行を末尾から減らし、減らしたことを伝える。
/// 運用画面のように全行が要る呼び出しは`ISUSCOPE_OUTPUT_BYTES=0`で外す。
const OUTPUT_BUDGET_BYTES: usize = 12_000;

/// 行を減らす対象と、減らしたときに勧める絞り方。
struct RowCap {
    /// トップレベルの行の配列。表にした後の`{columns, rows}`でもよい。
    field: &'static str,
    hint: &'static str,
}

const QUERY_CAP: RowCap = RowCap {
    field: "rows",
    hint: "narrow it with --node, --label, --label-contains or --limit (and --metric in the metric view)",
};
const SERIES_CAP: RowCap = RowCap {
    field: "rows",
    hint: "the last rows (later metrics and buckets) were dropped; narrow it with --metric, --node, --label or --from/--to",
};
const SQL_CAP: RowCap = RowCap {
    field: "rows",
    hint: "narrow it with WHERE or LIMIT",
};
const LIST_CAP: RowCap = RowCap {
    field: "runs",
    hint: "lower --limit",
};
const CHANGE_LIST_CAP: RowCap = RowCap {
    field: "changes",
    hint: "filter it with --status or lower --limit",
};

fn fit_rows(value: &mut serde_json::Value, cap: &RowCap) -> Result<()> {
    let budget = match env::var("ISUSCOPE_OUTPUT_BYTES") {
        Ok(text) => text
            .parse::<usize>()
            .context("ISUSCOPE_OUTPUT_BYTES must be a byte count (0 for no limit)")?,
        Err(_) => OUTPUT_BUDGET_BYTES,
    };
    let size = serde_json::to_vec(value)?.len();
    if budget == 0 || size <= budget {
        return Ok(());
    }
    let Some(rows) = rows_of(value, cap.field) else {
        return Ok(());
    };
    let shown = rows.len();
    let sizes = rows
        .iter()
        .map(|row| serde_json::to_vec(row).map(|bytes| bytes.len() + 1))
        .collect::<Result<Vec<_>, _>>()?;
    // 警告の文の分を残しておく。
    let mut used = size - sizes.iter().sum::<usize>() + 300;
    let mut kept = 0;
    for row_size in sizes {
        if used + row_size > budget {
            break;
        }
        used += row_size;
        kept += 1;
    }
    let kept = kept.max(1);
    if kept >= shown {
        return Ok(());
    }
    let message = format!(
        "output capped at {kept} of {shown} rows to fit the tool output limit; {}",
        cap.hint
    );
    if let Some(rows) = rows_of_mut(value, cap.field) {
        rows.truncate(kept);
    }
    // `truncated`は減らした表に立てる（`list`の`runs`のように名前の付いた表ならその中）。
    let table = match value.get_mut(cap.field) {
        Some(table @ serde_json::Value::Object(_)) => table,
        _ => &mut *value,
    };
    if let Some(table) = table.as_object_mut() {
        table.insert("truncated".into(), serde_json::Value::Bool(true));
    }
    let Some(fields) = value.as_object_mut() else {
        return Ok(());
    };
    match fields
        .entry("warnings")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
    {
        serde_json::Value::Array(warnings) => warnings.push(message.into()),
        other => *other = serde_json::Value::Array(vec![message.into()]),
    }
    Ok(())
}

fn rows_of<'a>(value: &'a serde_json::Value, field: &str) -> Option<&'a Vec<serde_json::Value>> {
    match value.get(field)? {
        serde_json::Value::Array(rows) => Some(rows),
        table => table.get("rows")?.as_array(),
    }
}

fn rows_of_mut<'a>(
    value: &'a mut serde_json::Value,
    field: &str,
) -> Option<&'a mut Vec<serde_json::Value>> {
    match value.get_mut(field)? {
        serde_json::Value::Array(rows) => Some(rows),
        table => table.get_mut("rows")?.as_array_mut(),
    }
}

/// 表にするfield。形ではなく名前で決めるのは、空の配列でも表の形（`columns`と`rows`）で出し、
/// 件数で出力の形が変わらないようにするため。行の中の短いlist（hostの`top_services`など）は、
/// 表を入れ子にすると読みにくいのでオブジェクトの並びのまま残す。
const TABLE_FIELDS: &[&str] = &[
    "rows",
    "items",
    "runs",
    "changes",
    "decisions",
    "conditions",
    "hosts",
    "clients",
];

/// 同じ形の行が並ぶ表は、列名を`columns`に1回だけ置き、各行を値の並びにする。`rows`と`items`は
/// 同じ階層に`columns`と`rows`を並べ（`common`はその前）、ほかの名前の表は`{columns, rows}`にする。
fn tabulate(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(items) => items.iter_mut().for_each(tabulate),
        serde_json::Value::Object(fields) => {
            let is_table = |name: &str, field: &serde_json::Value| {
                TABLE_FIELDS.contains(&name) && is_record_list(field)
            };
            let has_rows = ["rows", "items"]
                .iter()
                .any(|name| fields.get(*name).is_some_and(|field| is_table(name, field)));
            let mut common = has_rows.then(|| fields.shift_remove("common")).flatten();
            let mut output = serde_json::Map::new();
            for (name, mut field) in std::mem::take(fields) {
                if !is_table(&name, &field) {
                    tabulate(&mut field);
                    output.insert(name, field);
                    continue;
                }
                tabulate(&mut field);
                let (columns, rows) = table(field);
                if name == "rows" || name == "items" {
                    if let Some(common) = common.take() {
                        output.insert("common".into(), common);
                    }
                    output.insert("columns".into(), columns);
                    output.insert("rows".into(), rows);
                } else {
                    // 切らずに全件を出す表（hostのnode、変更の判断の履歴など）。ほかの表と同じく
                    // 件数と`truncated`を持たせる。
                    let total_count = rows.as_array().map_or(0, Vec::len);
                    output.insert(
                        name,
                        serde_json::json!({"total_count": total_count, "truncated": false, "columns": columns, "rows": rows}),
                    );
                }
            }
            *fields = output;
        }
        _ => {}
    }
}

fn is_record_list(value: &serde_json::Value) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().all(serde_json::Value::is_object))
}

fn table(value: serde_json::Value) -> (serde_json::Value, serde_json::Value) {
    let records = match value {
        serde_json::Value::Array(records) => records,
        _ => Vec::new(),
    };
    let mut columns = Vec::<String>::new();
    for record in &records {
        for name in record
            .as_object()
            .into_iter()
            .flat_map(|fields| fields.keys())
        {
            if !columns.contains(name) {
                columns.push(name.clone());
            }
        }
    }
    let rows = records
        .into_iter()
        .map(|mut record| {
            serde_json::Value::Array(
                columns
                    .iter()
                    .map(|name| {
                        record
                            .get_mut(name)
                            .map(serde_json::Value::take)
                            .unwrap_or(serde_json::Value::Null)
                    })
                    .collect(),
            )
        })
        .collect();
    (serde_json::json!(columns), serde_json::Value::Array(rows))
}

/// 小数は3桁まで（0.001未満は有効数字3桁）に丸め、件数のように小数部の無い値は`584.0`ではなく
/// `584`で書く。
fn round_numbers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Number(number) => {
            let Some(float) = number.as_f64().filter(|_| number.is_f64()) else {
                return;
            };
            let float = if float == 0.0 || float.abs() >= 0.001 {
                (float * 1000.0).round() / 1000.0
            } else {
                let scale = 10f64.powi(2 - float.abs().log10().floor() as i32);
                (float * scale).round() / scale
            };
            *value = if float.fract() == 0.0 && float.abs() < 9.0e15 {
                serde_json::Value::from(float as i64)
            } else {
                serde_json::Number::from_f64(float).map_or(serde_json::Value::Null, Into::into)
            };
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(round_numbers),
        serde_json::Value::Object(fields) => fields.values_mut().for_each(round_numbers),
        _ => {}
    }
}

fn show_brief(config: &LoadedConfig, requested: &str, limit: usize) -> Result<()> {
    let store = Store::open(&config.data_dir)?;
    let diagnostics = load_diagnostics(config, &store, requested)?;
    let review = store.run_review(&diagnostics.run)?;
    let id = diagnostics.run.id.clone();
    let id_for_score = id.clone();
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
    let score_metrics = store.query_metrics(&id_for_score, &[], Some("score."), Some(false))?;
    let score_inputs = query::metric_query(
        id_for_score,
        score_metrics,
        MetricQueryOptions {
            scope: QueryScope::Run,
            window: None,
            metrics: Vec::new(),
            metric_prefix: Some("score.".into()),
            node: None,
            source: None,
            labels: Vec::new(),
            label_contains: Vec::new(),
            group_by: Vec::new(),
            limit: usize::MAX,
        },
    );
    let mut brief = brief::build(diagnostics, benchmark, score_inputs, limit);
    brief.review = Some(brief::review(review));
    brief::next_steps(&mut brief);
    let mut brief = serde_json::to_value(&brief)?;
    for (section, columns) in BRIEF_COLUMNS {
        if let Some(items) = brief
            .get_mut(*section)
            .and_then(|section| section.get_mut("items"))
            .and_then(serde_json::Value::as_array_mut)
        {
            items
                .iter_mut()
                .for_each(|row| select_columns(row, columns));
        }
    }
    for (section, identity) in [
        ("benchmark", &["unit", "aggregation", "samples"][..]),
        ("score_inputs", &["unit", "aggregation", "samples"][..]),
        ("http", &["node", "method"][..]),
        ("database", &["node", "engine", "source", "window"][..]),
        ("cpu", &["node", "source"][..]),
        ("upstreams", &["node"][..]),
    ] {
        if let Some(section) = brief.get_mut(section) {
            hoist_common(section, identity);
        }
    }
    write_stdout_json(&brief)
}

/// briefの表に残す列と、その順序（行を識別する列を先に）。briefは順位を見て判断するための
/// ものなので主要な値だけにし、分位・最大・行数などの残りは`query`で見る。列が多いと、値の並びを
/// 列名へ対応させるときに読み違えやすい。
const BRIEF_COLUMNS: &[(&str, &[&str])] = &[
    (
        "http",
        &[
            "node", "method", "route", "count", "total_ms", "avg_ms", "p95_ms", "errors",
        ],
    ),
    (
        "database",
        &[
            "node",
            "engine",
            "source",
            "digest",
            "digest_id",
            "calls",
            "total_ms",
            "avg_ms",
            "p95_ms",
            "lock_ms",
            "rows_examined_per_call",
        ],
    ),
];

/// `columns`にある列だけを、その順に並べ直す。
fn select_columns(row: &mut serde_json::Value, columns: &[&str]) {
    let Some(fields) = row.as_object_mut() else {
        return;
    };
    let mut selected = serde_json::Map::new();
    for column in columns {
        if let Some(value) = fields.shift_remove(*column) {
            selected.insert((*column).into(), value);
        }
    }
    *fields = selected;
}

fn load_diagnostics(
    config: &LoadedConfig,
    store: &Store,
    requested: &str,
) -> Result<RunDiagnostics> {
    let id = store.require_id(requested, "run")?;
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
    fn named_tables_keep_their_shape_when_empty() {
        let mut value = serde_json::json!({
            "hosts": [],
            "clients": [{"node": "app1", "connections_in_use_max": 4}],
            "items": [],
            "top": [{"service": "nginx"}],
        });
        tabulate(&mut value);
        assert_eq!(
            value["hosts"],
            serde_json::json!({"total_count": 0, "truncated": false, "columns": [], "rows": []})
        );
        assert_eq!(
            value["clients"],
            serde_json::json!({"total_count": 1, "truncated": false, "columns": ["node", "connections_in_use_max"], "rows": [["app1", 4]]})
        );
        assert_eq!(value["columns"], serde_json::json!([]));
        assert_eq!(value["rows"], serde_json::json!([]));
        assert!(value.get("items").is_none());
        // 名前の無いlistはオブジェクトの並びのまま。
        assert_eq!(value["top"], serde_json::json!([{"service": "nginx"}]));
    }

    #[test]
    fn shared_identity_moves_to_common_once() {
        let mut value = serde_json::json!({"rows": [
            {"node": "app1", "route": "/a", "count": 1, "labels": {"collector": "alp", "quantile": "0.95"}},
            {"node": "app1", "route": "/b", "count": 2, "labels": {"collector": "alp", "quantile": "0.99"}},
        ]});
        hoist_common(&mut value, &["node", "route"]);
        assert_eq!(value["common"]["node"], "app1");
        assert_eq!(value["common"]["labels"]["collector"], "alp");
        assert!(value["common"].get("route").is_none());
        let row = &value["rows"][0];
        assert!(row.get("node").is_none());
        // 残った列は元の順のまま（末尾の列が前へ動かない）。
        assert_eq!(
            row.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["route", "count", "labels"]
        );
        assert_eq!(row["route"], "/a");
        assert_eq!(row["labels"], serde_json::json!({"quantile": "0.95"}));
        // 1行だけなら何もまとめない。
        let mut single = serde_json::json!({"rows": [{"node": "app1"}]});
        hoist_common(&mut single, &["node"]);
        assert!(single.get("common").is_none());
    }

    #[test]
    fn row_output_is_capped_to_the_budget_and_says_so() {
        let rows = (0..200)
            .map(|index| serde_json::json!({"digest": format!("select * from user_present_all_received_history where id = {index}"), "calls": index}))
            .collect::<Vec<_>>();
        let mut value = serde_json::json!({"total_count": 200, "truncated": false, "rows": rows});
        fit_rows(&mut value, &QUERY_CAP).unwrap();
        assert!(serde_json::to_vec(&value).unwrap().len() <= OUTPUT_BUDGET_BYTES);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["total_count"], 200);
        let kept = value["rows"].as_array().unwrap().len();
        assert!(kept > 0 && kept < 200);
        assert!(
            value["warnings"][0]
                .as_str()
                .unwrap()
                .contains(&format!("{kept} of 200"))
        );
        // 収まる出力には手を付けない。
        let mut small = serde_json::json!({"truncated": false, "rows": [{"calls": 1}]});
        fit_rows(&mut small, &QUERY_CAP).unwrap();
        assert_eq!(small["truncated"], false);
        assert!(small.get("warnings").is_none());
    }

    #[test]
    fn caps_reach_nested_tables_and_mark_the_table_they_cut() {
        let rows = (0..400)
            .map(|index| serde_json::json!([format!("run-{index:040}"), index]))
            .collect::<Vec<_>>();
        // `list`の`runs`は表にした後`{columns, rows}`になる。
        let mut list = serde_json::json!({"runs": {"columns": ["id", "score"], "rows": rows}});
        fit_rows(&mut list, &LIST_CAP).unwrap();
        let kept = list["runs"]["rows"].as_array().unwrap().len();
        assert!(kept > 0 && kept < 400);
        assert!(list["warnings"][0].as_str().unwrap().contains("--limit"));
        // 名前の付いた表では、減らした表の中に`truncated`を立てる。
        assert_eq!(list["runs"]["truncated"], true);
        let mut sql =
            serde_json::json!({"columns": ["id", "score"], "total_count": 400, "rows": rows});
        fit_rows(&mut sql, &SQL_CAP).unwrap();
        assert_eq!(sql["total_count"], 400);
        assert_eq!(sql["truncated"], true);
        assert!(sql["warnings"][0].as_str().unwrap().contains("WHERE"));
    }

    #[test]
    fn percent_only_query_loads_its_additive_dependency() {
        assert!(needs_cpu_sample_counts(
            &["cpu.sample_percent".into()],
            None,
            QueryScopeArg::Series
        ));
    }

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
    fn buckets_are_counted_from_the_exact_window_start() {
        // 負荷の始まり1012.8秒から4.4秒後（1017.2秒）は最初のbucket。整数秒へ切ってから引くと
        // (1017 - 1012) / 5 = 1になり、hostのsampleとHTTPのbucketが別の行へずれていた。
        let start = chrono::DateTime::from_timestamp_micros(1_012_800_000).unwrap();
        let at = |micros| chrono::DateTime::from_timestamp_micros(micros).unwrap();
        assert_eq!(bucket_index(at(1_017_200_000), start, 5), Some(0));
        assert_eq!(bucket_index(at(1_017_800_000), start, 5), Some(1));
        assert_eq!(bucket_index(at(1_012_799_999), start, 5), Some(-1));
    }

    #[test]
    fn overview_reads_the_database_totals_from_slp_before_per_statement_rows() {
        // slp collectorはDB全体の5秒bucketを`db.calls`・`db.duration`で出す。
        let slp = BucketRow {
            db_total_calls: Some(42.0),
            db_total_time: Some(840.0),
            ..Default::default()
        };
        assert_eq!(slp.database(), (Some(42.0), Some(840.0)));
        // 旧来のSQL別bucketしか無いrunは、その合計。
        let legacy = BucketRow {
            db_calls: 3.0,
            db_time: 9.0,
            ..Default::default()
        };
        assert_eq!(legacy.database(), (Some(3.0), Some(9.0)));
        // 両方ある（新旧のcollectorが混ざった設定）なら、DB全体の値だけを使い二重に数えない。
        let both = BucketRow {
            db_calls: 3.0,
            db_time: 9.0,
            ..slp
        };
        assert_eq!(both.database(), (Some(42.0), Some(840.0)));
        assert_eq!(BucketRow::default().database(), (None, None));
    }

    #[test]
    fn sparse_cpu_percentages_are_recomputed_per_series_bucket() {
        let start = chrono::DateTime::from_timestamp(0, 0).unwrap();
        let metrics = [
            ("A", 0, 100.0),
            ("B", 5, 100.0),
            ("A", 10, 25.0),
            ("B", 15, 75.0),
        ]
        .into_iter()
        .flat_map(|(symbol, second, count)| {
            [
                ("cpu.sample_count", "samples", count),
                ("cpu.sample_percent", "percent", 100.0),
            ]
            .map(|(name, unit, value)| isuscope::model::Metric {
                name: name.into(),
                unit: unit.into(),
                value,
                timestamp: Some(start + chrono::Duration::seconds(second)),
                labels: BTreeMap::from([
                    ("node".into(), "app".into()),
                    ("collector".into(), "perf".into()),
                    ("symbol".into(), symbol.into()),
                ]),
            })
        })
        .collect::<Vec<_>>();
        for labels in [vec![], vec![("symbol".into(), "A".into())]] {
            let options = SeriesOptions {
                metrics: vec!["cpu.sample_percent".into()],
                metric_prefix: None,
                source: None,
                node: None,
                labels,
                label_contains: Vec::new(),
                from: 0,
                to: None,
                window: SeriesWindowArg::Load,
                bucket: 10,
                limit: 100,
            };
            let end = start + chrono::Duration::seconds(20);
            let selected = metrics
                .iter()
                .filter(|metric| metric_matches(metric, &options, start, end))
                .cloned()
                .collect();
            let SeriesData::Metrics { rows, .. } =
                generic_series_data(start, start, end, &options, selected)
            else {
                panic!("expected metrics")
            };
            assert_eq!(rows.len(), if options.labels.is_empty() { 4 } else { 2 });
            for row in rows {
                assert_eq!(row.metric, "cpu.sample_percent");
                let expected = if row.from_seconds == 0 {
                    50.0
                } else if row.labels["symbol"] == "A" {
                    25.0
                } else {
                    75.0
                };
                assert_eq!(row.value, expected);
            }
        }
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
                metric_prefix: None,
                source: None,
                node: None,
                labels: Vec::new(),
                label_contains: Vec::new(),
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
