use crate::model::{FileDigest, SourceSnapshot};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use walkdir::WalkDir;

pub fn capture(repo: &Path, output_dir: &Path, excludes: &[PathBuf]) -> Result<SourceSnapshot> {
    fs::create_dir_all(output_dir)?;
    match git_output(repo, &["rev-parse", "--show-toplevel"]) {
        Ok(top) => capture_git(repo, output_dir, top.trim(), excludes),
        Err(error) => capture_without_git(repo, output_dir, excludes, error.to_string()),
    }
}

fn capture_git(
    repo: &Path,
    output_dir: &Path,
    top: &str,
    excludes: &[PathBuf],
) -> Result<SourceSnapshot> {
    let root = PathBuf::from(top);
    let commit_hash = git_output(repo, &["rev-parse", "HEAD"]).ok().map(trimmed);
    let branch = git_output(repo, &["branch", "--show-current"])
        .ok()
        .map(trimmed)
        .filter(|value| !value.is_empty());
    let status = git_scoped_output(repo, &["status", "--porcelain=v1", "-z"], excludes)?;
    let dirty = !status.is_empty();
    let patch =
        git_scoped_output(repo, &["diff", "--binary", "HEAD"], excludes).unwrap_or_default();
    fs::write(output_dir.join("working-tree.patch"), &patch)?;

    let untracked_raw =
        git_output_bytes(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut untracked = Vec::new();
    for raw_path in untracked_raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
    {
        let relative = String::from_utf8_lossy(raw_path).into_owned();
        if is_excluded(Path::new(&relative), excludes) {
            continue;
        }
        let path = root.join(&relative);
        if path.is_file() {
            untracked.push(FileDigest {
                path: relative,
                sha256: sha256_file(&path)?,
            });
        }
    }
    untracked.sort_by(|left, right| left.path.cmp(&right.path));

    let mut state = Sha256::new();
    if let Some(commit) = &commit_hash {
        state.update(commit.as_bytes());
    }
    state.update(&patch);
    for file in &untracked {
        state.update(file.path.as_bytes());
        state.update(file.sha256.as_bytes());
    }

    let snapshot = SourceSnapshot {
        repository: root.display().to_string(),
        git_available: true,
        commit_hash,
        branch,
        dirty,
        state_sha256: format!("{:x}", state.finalize()),
        untracked,
        error: None,
    };
    write_snapshot(output_dir, &snapshot)?;
    Ok(snapshot)
}

fn capture_without_git(
    repo: &Path,
    output_dir: &Path,
    excludes: &[PathBuf],
    git_error: String,
) -> Result<SourceSnapshot> {
    let mut files = Vec::new();
    let walker = WalkDir::new(repo)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            let Ok(relative) = entry.path().strip_prefix(repo) else {
                return false;
            };
            relative.as_os_str().is_empty() || !is_excluded(relative, excludes)
        });
    for entry in walker {
        let entry = entry?;
        let path = entry.path();
        let relative = match path.strip_prefix(repo) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        if path.is_file() {
            files.push(FileDigest {
                path: relative.display().to_string(),
                sha256: sha256_file(path)?,
            });
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let mut state = Sha256::new();
    for file in &files {
        state.update(file.path.as_bytes());
        state.update(file.sha256.as_bytes());
    }
    let snapshot = SourceSnapshot {
        repository: repo.display().to_string(),
        git_available: false,
        commit_hash: None,
        branch: None,
        dirty: true,
        state_sha256: format!("{:x}", state.finalize()),
        untracked: files,
        error: Some(git_error),
    };
    write_snapshot(output_dir, &snapshot)?;
    Ok(snapshot)
}

fn is_excluded(relative: &Path, configured: &[PathBuf]) -> bool {
    relative.components().any(|part| {
        matches!(
            part.as_os_str().to_str(),
            Some(".git" | ".isuscope" | "target")
        )
    }) || configured
        .iter()
        .any(|exclude| relative.starts_with(exclude))
}

fn write_snapshot(output_dir: &Path, snapshot: &SourceSnapshot) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(snapshot)?;
    fs::write(output_dir.join("git.json"), bytes)?;
    Ok(())
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String> {
    let bytes = git_output_bytes(repo, args)?;
    String::from_utf8(bytes).context("git returned non-UTF-8 output")
}

fn git_output_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("cannot execute git in {}", repo.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn git_scoped_output(repo: &Path, args: &[&str], excludes: &[PathBuf]) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.args(args).arg("--").arg(".");
    for exclude in excludes {
        command.arg(format!(
            ":(exclude){}",
            exclude.to_string_lossy().replace('\\', "/")
        ));
    }
    let output = command
        .current_dir(repo)
        .output()
        .with_context(|| format!("cannot execute git in {}", repo.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn trimmed(value: String) -> String {
    value.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    #[test]
    fn captures_commit_and_dirty_patch() {
        let dir = tempdir().unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        fs::write(dir.path().join("app.txt"), "before\n").unwrap();
        Command::new("git")
            .args(["add", "app.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();
        fs::write(dir.path().join("app.txt"), "after\n").unwrap();
        let out = dir.path().join("snapshot");
        let snapshot = capture(dir.path(), &out, &[]).unwrap();
        assert!(snapshot.git_available);
        assert!(snapshot.dirty);
        assert!(snapshot.commit_hash.is_some());
        assert!(
            fs::read_to_string(out.join("working-tree.patch"))
                .unwrap()
                .contains("after")
        );
    }

    #[test]
    fn configured_excludes_do_not_mark_a_git_snapshot_dirty() {
        let dir = tempdir().unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        fs::create_dir_all(dir.path().join("docs/history")).unwrap();
        fs::write(dir.path().join("app.txt"), "app\n").unwrap();
        fs::write(dir.path().join("docs/history/session.md"), "before\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .status()
            .unwrap();
        Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();
        fs::write(dir.path().join("docs/history/session.md"), "after\n").unwrap();
        fs::write(dir.path().join("docs/history/untracked.md"), "new\n").unwrap();

        let out = dir.path().join("snapshot");
        let snapshot = capture(
            dir.path(),
            &out,
            &[PathBuf::from("docs/history"), PathBuf::from("snapshot")],
        )
        .unwrap();
        assert!(!snapshot.dirty);
        assert!(snapshot.untracked.is_empty());
        assert!(fs::read(out.join("working-tree.patch")).unwrap().is_empty());
    }
}
