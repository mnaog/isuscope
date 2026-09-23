//! Repository-side helpers for saved runs: staging raw logs and proposing route rules.

use crate::{config::LoadedConfig, storage::Store};
use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::Command,
};

/// Stages a run in Git including its raw `logs/`, which projects normally ignore.
pub fn pin(config: &LoadedConfig, requested: &str) -> Result<String> {
    let store = Store::open(&config.data_dir)?;
    let id = store
        .resolve_id(requested)?
        .with_context(|| format!("run not found: {requested}"))?;
    // Keep enrich/analyze from changing the manifest (and therefore its dependency set)
    // between resolving the run and staging it.
    let _lock = store.lock_run(&id)?;
    let run_dir = store.final_dir(&id);
    let repo = config.source_repo();
    let repo = repo.canonicalize().unwrap_or(repo);
    let run_dir = run_dir.canonicalize().unwrap_or(run_dir);
    let relative = run_dir
        .strip_prefix(&repo)
        .with_context(|| format!("run {} is outside source repository {}", id, repo.display()))?;
    if !run_dir.join("run.json").is_file() {
        bail!("run.json not found: {}", relative.display());
    }
    if !run_dir.join("logs").is_dir() {
        bail!("logs not found: {}/logs", relative.display());
    }
    // Stage the complete run dependency tree even when the data directory is ignored. `-A`
    // also removes snapshots and parser logs from an older enrichment generation from the index.
    git_add(&repo, &["-f", "-A"], &[relative.to_path_buf()])?;
    Ok(id)
}

fn git_add(repo: &Path, flags: &[&str], paths: &[std::path::PathBuf]) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("add")
        .args(flags)
        .arg("--")
        .args(paths)
        .status()
        .context("cannot run git add")?;
    if !status.success() {
        bail!("git add failed with {status}");
    }
    Ok(())
}

const SEGMENTS: &[(&str, &str, &str)] = &[
    (r"^[0-9]+$", ":id", "[0-9]+"),
    (
        r"^[0-9a-fA-F]{8}-[0-9a-fA-F-]{27,}$",
        ":uuid",
        "[0-9A-Fa-f-]+",
    ),
    (r"^[0-9a-fA-F]{16,}$", ":key", "[0-9A-Fa-f]+"),
    (r"^[A-Za-z0-9_-]{24,}$", ":key", "[A-Za-z0-9_-]+"),
];

fn normalize(route: &str, matchers: &[(regex::Regex, &str, &str)]) -> Option<(String, String)> {
    let mut canonical = Vec::new();
    let mut pattern = Vec::new();
    let mut changed = false;
    for segment in route.split('/').skip(1) {
        if let Some((_, label, expression)) = matchers
            .iter()
            .find(|(matcher, _, _)| matcher.is_match(segment))
        {
            canonical.push((*label).to_owned());
            pattern.push((*expression).to_owned());
            changed = true;
        } else {
            canonical.push(segment.to_owned());
            pattern.push(regex::escape(segment));
        }
    }
    changed.then(|| {
        (
            format!("^/{}$", pattern.join("/")),
            format!("/{}", canonical.join("/")),
        )
    })
}

/// Proposes `[[routes]]` rules for HTTP routes of a run that still contain dynamic segments.
/// Returns the TOML text and the number of rules; nothing is written to `routes.toml`.
pub fn suggest_routes(config: &LoadedConfig, requested: &str) -> Result<(String, usize)> {
    let store = Store::open(&config.data_dir)?;
    let id = store
        .resolve_id(requested)?
        .with_context(|| format!("run not found: {requested}"))?;
    let matchers = SEGMENTS
        .iter()
        .map(|(matcher, label, expression)| Ok((regex::Regex::new(matcher)?, *label, *expression)))
        .collect::<Result<Vec<_>>>()?;
    let mut candidates = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for metric in store.query_metrics(&id, &["http.requests".into()], None, None)? {
        let Some(route) = metric.labels.get("route") else {
            continue;
        };
        if let Some(key) = normalize(route, &matchers) {
            candidates.entry(key).or_default().insert(route.clone());
        }
    }
    let mut output =
        String::from("# Generated candidates. Review before copying to .isuscope/routes.toml.\n");
    for ((pattern, canonical), examples) in &candidates {
        let examples = examples
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            "\n# examples: {examples}\n[[routes]]\npattern = {}\nreplace = {}\n",
            toml_string(pattern),
            toml_string(canonical)
        ));
    }
    Ok((output, candidates.len()))
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("strings always serialize")
}

pub fn write_output(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content).with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_segments_become_rules() {
        let matchers = SEGMENTS
            .iter()
            .map(|(matcher, label, expression)| {
                (regex::Regex::new(matcher).unwrap(), *label, *expression)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            normalize("/users/42/profile", &matchers),
            Some((
                "^/users/[0-9]+/profile$".into(),
                "/users/:id/profile".into()
            ))
        );
        assert_eq!(normalize("/login", &matchers), None);
        assert_eq!(
            normalize("/items/0123456789abcdef0123", &matchers).map(|rule| rule.1),
            Some("/items/:key".into())
        );
    }
}
