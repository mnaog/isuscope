use anyhow::{Context, Result};
use std::{fs, path::Path};

const CONFIG: &str = include_str!("../templates/config.toml");

/// Values that differ per project. `init` fills them with defaults; the starter template
/// passes the paths its Ansible role installed. The collectors themselves stay one file.
#[derive(Debug, Clone)]
pub struct ConfigOptions {
    pub data_dir: String,
    pub nginx_access_log: String,
    pub mysql_slow_log: String,
    pub service_units: Vec<String>,
    pub sample_output: Option<String>,
    /// Append commented `[lock]`, `[ssh]` and `[[nodes]]` examples. A project that adds its
    /// own sections (the starter template does) turns this off to avoid duplicate tables.
    pub scaffold: bool,
}

impl Default for ConfigOptions {
    fn default() -> Self {
        Self {
            data_dir: ".isuscope/data".into(),
            nginx_access_log: "/var/log/nginx/access.log".into(),
            mysql_slow_log: "/var/log/mysql/mysql-slow.log".into(),
            service_units: Vec::new(),
            sample_output: None,
            scaffold: true,
        }
    }
}

const SCAFFOLD: &str = r#"
# deployなどと共有する変更系操作のlock。runとsurvey-runはベンチ中これを保持します。
# [lock]
# path = ".local/operation.lock"

[ssh]
user = "ubuntu"
connect_timeout_seconds = 5
# collectorが運ぶlog差分はtextで、ベンチ後のnodeは空いている。既定で圧縮します。
# compression = true
# projectのknown_hostsを使う場合に指定します。未登録hostは初回だけ受け入れて固定します。
# known_hosts_file = ".local/known-hosts"

# [[nodes]]はSETUP.mdの手順に従って追加します。roleを指定しない標準collectorは設定した
# 全nodeで、roleを指定したcollectorはそのroleを持つnodeだけで動きます。
# [[nodes]]
# name = "app1"
# host = "10.0.0.1"
# roles = ["app", "nginx", "db"]

# benchmarker自身が動くnodeを書く場合は`rule_side = true`を付けます。collectorもdisk検査も
# そのnodeでは動かず、ベンチ側を計測しません。
# [[nodes]]
# name = "bench"
# host = "10.0.0.9"
# rule_side = true
"#;

pub fn render_config(options: &ConfigOptions) -> String {
    let units = options
        .service_units
        .iter()
        .map(|unit| format!("{unit:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sample = match &options.sample_output {
        Some(path) => format!("sample_output = {path:?}"),
        None => "# sample_output = \"config/benchmark-sample.log\"".into(),
    };
    let rendered = CONFIG
        .replace("@@DATA_DIR@@", &options.data_dir)
        .replace("@@NGINX_ACCESS_LOG@@", &options.nginx_access_log)
        .replace("@@MYSQL_SLOW_LOG@@", &options.mysql_slow_log)
        .replace("@@SERVICE_UNITS@@", &units)
        .replace("@@SAMPLE_OUTPUT@@", &sample);
    if options.scaffold {
        format!("{rendered}{SCAFFOLD}")
    } else {
        rendered
    }
}
const ROUTES: &str = include_str!("../templates/routes.toml");
const SETUP: &str = include_str!("../templates/setup.sh");
const SETUP_DOC: &str = include_str!("../templates/SETUP.md");
const FINGERPRINT: &str = include_str!("../templates/fingerprint.sh");
const BENCHMARK: &str = include_str!("../templates/benchmark.sh");
const BENCHMARK_PARSER: &str = include_str!("../templates/parse-benchmark.sh");

pub fn scaffold(project_root: &Path) -> Result<()> {
    scaffold_with(project_root, &ConfigOptions::default())
}

pub fn scaffold_with(project_root: &Path, options: &ConfigOptions) -> Result<()> {
    let directory = project_root.join(".isuscope");
    fs::create_dir_all(&directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    create_if_missing(&directory.join("config.toml"), &render_config(options))?;
    create_if_missing(&directory.join("routes.toml"), ROUTES)?;
    let setup = directory.join("setup.sh");
    let setup_created = create_if_missing(&setup, SETUP)?;
    let fingerprint = directory.join("fingerprint.sh");
    let fingerprint_created = create_if_missing(&fingerprint, FINGERPRINT)?;
    let benchmark = directory.join("benchmark.sh");
    let benchmark_created = create_if_missing(&benchmark, BENCHMARK)?;
    let benchmark_parser = directory.join("parse-benchmark.sh");
    let benchmark_parser_created = create_if_missing(&benchmark_parser, BENCHMARK_PARSER)?;
    create_if_missing(&directory.join("SETUP.md"), SETUP_DOC)?;
    #[cfg(unix)]
    if setup_created {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&setup)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&setup, permissions)?;
    }
    #[cfg(unix)]
    if fingerprint_created {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&fingerprint)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fingerprint, permissions)?;
    }
    #[cfg(unix)]
    if benchmark_created {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&benchmark)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&benchmark, permissions)?;
    }
    #[cfg(unix)]
    if benchmark_parser_created {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&benchmark_parser)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&benchmark_parser, permissions)?;
    }
    println!("isuscope scaffold: {}", directory.display());
    println!(
        "next: inspect .isuscope/SETUP.md; do not run setup.sh until its checklist is complete"
    );
    Ok(())
}

fn create_if_missing(path: &Path, contents: &str) -> Result<bool> {
    if path.exists() {
        println!("keep   {}", path.display());
        return Ok(false);
    }
    fs::write(path, contents).with_context(|| format!("cannot write {}", path.display()))?;
    println!("create {}", path.display());
    Ok(true)
}
