use crate::{config::LoadedConfig, model::ToolingSnapshot};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Component, path::Path};

pub fn capture(config: &LoadedConfig, destination: &Path) -> Result<ToolingSnapshot> {
    fs::create_dir_all(destination)?;
    let mut config_sha256 = copy_and_hash(&config.config_path, &destination.join("config.toml"))?;
    // `[include] config`のfileも設定の一部なので、写しを残し、`config_sha256`へ含める。
    // 含めるfileが無いprojectのhashは変わらない。
    if !config.include_paths.is_empty() {
        let mut combined = Sha256::new();
        combined.update(config_sha256.as_bytes());
        for (index, include) in config.include_paths.iter().enumerate() {
            let name = include
                .file_name()
                .context("included configuration has no file name")?;
            let target = destination
                .join("include")
                .join(format!("{index}-{}", name.to_string_lossy()));
            fs::create_dir_all(destination.join("include"))?;
            combined.update(copy_and_hash(include, &target)?.as_bytes());
        }
        config_sha256 = format!("{:x}", combined.finalize());
    }
    let config_dir = config
        .config_path
        .parent()
        .context("configuration path has no parent")?;
    let routes_sha256 = copy_optional(
        &config_dir.join("routes.toml"),
        &destination.join("routes.toml"),
    )?;
    let setup_script_sha256 =
        copy_optional(&config_dir.join("setup.sh"), &destination.join("setup.sh"))?;
    let setup_state_sha256 = copy_optional(
        &config_dir.join("setup-state.json"),
        &destination.join("setup-state.json"),
    )?;
    let mut extra_files_sha256 = BTreeMap::new();
    for relative in &config.config.tooling.include {
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            anyhow::bail!(
                "tooling include must be a relative file path: {}",
                relative.display()
            );
        }
        let source = config_dir.join(relative);
        let target = destination.join("extra").join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let hash = copy_and_hash(&source, &target)?;
        extra_files_sha256.insert(relative.display().to_string(), hash);
    }
    fs::write(
        destination.join("isuscope-version.txt"),
        format!("isuscope {}\n", env!("CARGO_PKG_VERSION")),
    )?;
    Ok(ToolingSnapshot {
        isuscope_version: env!("CARGO_PKG_VERSION").into(),
        config_sha256,
        routes_sha256,
        setup_script_sha256,
        setup_state_sha256,
        extra_files_sha256,
        error: None,
    })
}

fn copy_optional(source: &Path, destination: &Path) -> Result<Option<String>> {
    if !source.is_file() {
        return Ok(None);
    }
    copy_and_hash(source, destination).map(Some)
}

fn copy_and_hash(source: &Path, destination: &Path) -> Result<String> {
    let bytes = fs::read(source).with_context(|| format!("cannot read {}", source.display()))?;
    fs::write(destination, &bytes)
        .with_context(|| format!("cannot write {}", destination.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}
