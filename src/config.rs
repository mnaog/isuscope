use crate::model::RunMode;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub source: SourceConfig,
    #[serde(default)]
    pub tooling: ToolingConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    pub benchmark: BenchmarkConfig,
    #[serde(default)]
    pub ssh: SshConfig,
    #[serde(default)]
    pub nodes: Vec<NodeConfig>,
    #[serde(default)]
    pub collectors: Vec<CollectorConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ObservabilityConfig {
    /// systemd units sampled directly from their cgroup v2 files.
    #[serde(default)]
    pub service_units: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContextConfig {
    pub codex: Option<CodexContextConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexContextConfig {
    pub history_dir: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolingConfig {
    #[serde(default)]
    pub include: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub config_path: PathBuf,
    pub project_root: PathBuf,
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceConfig {
    #[serde(default = "default_repo")]
    pub repo: PathBuf,
    #[serde(default)]
    pub exclude: Vec<PathBuf>,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            repo: default_repo(),
            exclude: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BenchmarkMode {
    Command,
    External,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BenchmarkConfig {
    #[serde(default = "default_benchmark_mode")]
    pub mode: BenchmarkMode,
    #[serde(default)]
    pub command: Vec<String>,
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub stream_output: bool,
    #[serde(default = "default_score_pattern")]
    pub score_pattern: String,
    #[serde(default = "default_initialize_start_marker")]
    pub initialize_start_marker: String,
    #[serde(default = "default_initialize_finish_marker")]
    pub initialize_finish_marker: String,
    #[serde(default)]
    pub parsers: Vec<BenchmarkParserConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BenchmarkParserConfig {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default = "default_parser_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_parser_output_bytes")]
    pub max_output_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SshConfig {
    #[serde(default = "default_ssh_user")]
    pub user: String,
    pub identity_file: Option<PathBuf>,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_seconds: u64,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            user: default_ssh_user(),
            identity_file: None,
            connect_timeout_seconds: default_connect_timeout(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeConfig {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub roles: Vec<String>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CollectorPhase {
    Before,
    During,
    After,
}

impl CollectorPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::During => "during",
            Self::After => "after",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    #[default]
    Local,
    Ssh,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CollectorParser {
    AlpJson,
    MysqlSlow,
    SlpJson,
    SlpTsv,
    Sysstat,
    ServiceCgroup,
    PerfScript,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CollectorConfig {
    pub name: String,
    pub phase: CollectorPhase,
    #[serde(default)]
    pub transport: Transport,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub modes: Vec<RunMode>,
    pub command: Vec<String>,
    /// Optional adapter for a standard tool's native stdout.
    pub parser: Option<CollectorParser>,
    #[serde(default = "default_collector_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: u64,
    #[serde(default)]
    pub required: bool,
    #[serde(default = "default_unavailable_exit_codes")]
    pub unavailable_exit_codes: Vec<i32>,
}

impl CollectorConfig {
    pub fn enabled_for(&self, mode: RunMode) -> bool {
        self.modes.is_empty() || self.modes.contains(&mode)
    }
}

fn default_data_dir() -> PathBuf {
    PathBuf::from(".isuscope")
}
fn default_repo() -> PathBuf {
    PathBuf::from(".")
}
fn default_benchmark_mode() -> BenchmarkMode {
    BenchmarkMode::Command
}
fn default_score_pattern() -> String {
    String::from(r"スコア:\s*([0-9]+)")
}
fn default_initialize_start_marker() -> String {
    String::from("webappの初期化を行います")
}
fn default_initialize_finish_marker() -> String {
    String::from("ベンチマーク走行前のデータ整合性チェック")
}
fn default_ssh_user() -> String {
    String::from("ubuntu")
}
fn default_connect_timeout() -> u64 {
    5
}
fn default_collector_timeout() -> u64 {
    90
}
fn default_max_output_bytes() -> u64 {
    1024 * 1024 * 1024
}
fn default_unavailable_exit_codes() -> Vec<i32> {
    vec![75]
}
fn default_parser_timeout() -> u64 {
    30
}
fn default_parser_output_bytes() -> u64 {
    16 * 1024 * 1024
}

impl LoadedConfig {
    pub fn discover(start: &Path) -> Result<Self> {
        let start = start
            .canonicalize()
            .context("cannot resolve current directory")?;
        for dir in start.ancestors() {
            let candidate = dir.join(".isuscope/config.toml");
            if candidate.is_file() {
                return Self::load(&candidate);
            }
        }
        bail!("`.isuscope/config.toml` was not found in this directory or its parents")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let config: Config = toml::from_str(&raw)
            .with_context(|| format!("invalid configuration in {}", path.display()))?;
        validate(&config)?;
        let config_dir = path.parent().context("config path has no parent")?;
        let project_root = config_dir
            .parent()
            .context(".isuscope must be inside a project directory")?
            .to_path_buf();
        let data_dir = resolve(&project_root, &config.data_dir);
        Ok(Self {
            config,
            config_path: path.to_path_buf(),
            project_root,
            data_dir,
        })
    }

    pub fn source_repo(&self) -> PathBuf {
        resolve(&self.project_root, &self.config.source.repo)
    }

    pub fn benchmark_working_dir(&self) -> PathBuf {
        self.config
            .benchmark
            .working_dir
            .as_ref()
            .map(|path| resolve(&self.project_root, path))
            .unwrap_or_else(|| self.project_root.clone())
    }

    pub fn codex_history_dir(&self) -> Option<PathBuf> {
        self.config
            .context
            .codex
            .as_ref()
            .map(|codex| resolve(&self.source_repo(), &codex.history_dir))
    }
}

fn validate(config: &Config) -> Result<()> {
    if let Some(codex) = &config.context.codex
        && (codex.history_dir.as_os_str().is_empty()
            || codex.history_dir.is_absolute()
            || codex
                .history_dir
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_))))
    {
        bail!("context.codex.history_dir must be a non-empty relative path without `..`");
    }
    if matches!(config.benchmark.mode, BenchmarkMode::Command)
        && config.benchmark.command.is_empty()
    {
        bail!("benchmark.command must not be empty when benchmark.mode is `command`");
    }
    let mut node_names = std::collections::BTreeSet::new();
    for node in &config.nodes {
        if !node_names.insert(&node.name) {
            bail!("duplicate node name `{}`", node.name);
        }
    }
    let mut parser_names = std::collections::BTreeSet::new();
    for parser in &config.benchmark.parsers {
        if parser.name.trim().is_empty() {
            bail!("benchmark parser name must not be empty");
        }
        if parser.name == "inline" {
            bail!("benchmark parser name `inline` is reserved");
        }
        if !parser
            .name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        {
            bail!(
                "benchmark parser name `{}` may contain only ASCII letters, digits, `-`, and `_`",
                parser.name
            );
        }
        if parser.command.is_empty() {
            bail!("benchmark parser `{}` has an empty command", parser.name);
        }
        if !parser_names.insert(&parser.name) {
            bail!("duplicate benchmark parser name `{}`", parser.name);
        }
    }
    let mut collector_keys = std::collections::BTreeSet::new();
    for collector in &config.collectors {
        if collector.command.is_empty() {
            bail!("collector `{}` has an empty command", collector.name);
        }
        let key = format!("{}:{}", collector.name, collector.phase.as_str());
        if !collector_keys.insert(key) {
            bail!(
                "collector `{}` is duplicated in phase `{}`",
                collector.name,
                collector.phase.as_str()
            );
        }
    }
    for unit in &config.observability.service_units {
        if unit.is_empty()
            || !unit.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '@' | '-')
            })
        {
            bail!(
                "observability service unit `{unit}` may contain only ASCII letters, digits, `.`, `_`, `@`, and `-`"
            );
        }
    }
    Ok(())
}

pub fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}
