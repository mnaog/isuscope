use anyhow::{Context, Result};
use std::{fs, path::Path};

const CONFIG: &str = include_str!("../templates/config.toml");
const ROUTES: &str = include_str!("../templates/routes.toml");
const SETUP: &str = include_str!("../templates/setup.sh");
const SETUP_DOC: &str = include_str!("../templates/SETUP.md");
const FINGERPRINT: &str = include_str!("../templates/fingerprint.sh");
const BENCHMARK: &str = include_str!("../templates/benchmark.sh");
const BENCHMARK_PARSER: &str = include_str!("../templates/parse-benchmark.sh");

pub fn scaffold(project_root: &Path) -> Result<()> {
    let directory = project_root.join(".isuscope");
    fs::create_dir_all(&directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    create_if_missing(&directory.join("config.toml"), CONFIG)?;
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
