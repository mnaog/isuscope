use crate::{config::LoadedConfig, model::CodexContext};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{env, fs, path::Path};

pub const SNAPSHOT_PATH: &str = "context/codex-history.md";

pub struct ResolvedCodexContext {
    pub metadata: CodexContext,
    bytes: Vec<u8>,
}

impl ResolvedCodexContext {
    pub fn write_snapshot(&self, run_dir: &Path) -> Result<()> {
        let destination = run_dir.join(SNAPSHOT_PATH);
        let parent = destination
            .parent()
            .context("Codex context snapshot has no parent directory")?;
        fs::create_dir_all(parent)?;
        fs::write(&destination, &self.bytes)
            .with_context(|| format!("cannot write {}", destination.display()))
    }
}

pub fn current_session_id() -> Result<String> {
    let session = env::var("CODEX_SESSION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let thread = env::var("CODEX_THREAD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if let (Some(session), Some(thread)) = (&session, &thread)
        && session != thread
    {
        bail!(
            "CODEX_SESSION_ID and CODEX_THREAD_ID disagree; refusing to attach ambiguous Codex context"
        );
    }
    session
        .or(thread)
        .map(|value| safe_component(&value))
        .context(
            "Codex context is required, but CODEX_SESSION_ID/CODEX_THREAD_ID is not set; run the benchmark from Codex",
        )
}

fn safe_component(value: &str) -> String {
    let mut cleaned = String::new();
    let mut replaced = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-') {
            cleaned.push(character);
            replaced = false;
        } else if !replaced {
            cleaned.push('-');
            replaced = true;
        }
    }
    let cleaned = cleaned.trim_matches(['-', '.']);
    let cleaned = if cleaned.is_empty() {
        "unknown-session"
    } else {
        cleaned
    };
    if cleaned.len() <= 96 {
        cleaned.to_owned()
    } else {
        let digest = format!("{:x}", Sha256::digest(value.as_bytes()));
        format!("{}-{}", &cleaned[..80], &digest[..12])
    }
}

pub fn resolve(config: &LoadedConfig) -> Result<Option<ResolvedCodexContext>> {
    let Some(history_dir) = config.codex_history_dir() else {
        return Ok(None);
    };
    let session_id = current_session_id()?;
    let source_repo = config
        .source_repo()
        .canonicalize()
        .context("cannot resolve source repository for Codex context")?;
    let history_dir = history_dir.canonicalize().with_context(|| {
        format!(
            "cannot resolve Codex history directory {}",
            history_dir.display()
        )
    })?;
    if !history_dir.starts_with(&source_repo) {
        bail!(
            "Codex history directory must remain inside source repository {}",
            source_repo.display()
        );
    }

    let mut matches = Vec::new();
    for entry in fs::read_dir(&history_dir).with_context(|| {
        format!(
            "cannot read Codex history directory {}",
            history_dir.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("md")
        {
            continue;
        }
        let bytes = fs::read(&path)
            .with_context(|| format!("cannot read Codex history {}", path.display()))?;
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("Codex history is not UTF-8: {}", path.display()))?;
        if history_belongs_to_session(text, &session_id) {
            let input_id = latest_user_input_id(text, &session_id);
            matches.push((path, bytes, input_id));
        }
    }
    if matches.is_empty() {
        bail!(
            "no Codex history file in {} belongs to current session `{}`; ensure the UserPromptSubmit hook was active before starting this session",
            history_dir.display(),
            session_id
        );
    }
    if matches.len() > 1 {
        bail!(
            "multiple Codex history files in {} belong to current session `{}`",
            history_dir.display(),
            session_id
        );
    }
    let (path, bytes, input_id) = matches.pop().expect("one history match was checked");
    let input_id = input_id.with_context(|| {
        format!(
            "Codex history {} has no User input marker for current session `{}`",
            path.display(),
            session_id
        )
    })?;
    let relative = path.strip_prefix(&source_repo).with_context(|| {
        format!(
            "Codex history {} is outside source repository {}",
            path.display(),
            source_repo.display()
        )
    })?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    Ok(Some(ResolvedCodexContext {
        metadata: CodexContext {
            history_path: relative.display().to_string(),
            session_id,
            input_id,
            snapshot_path: SNAPSHOT_PATH.into(),
            sha256,
        },
        bytes,
    }))
}

pub fn valid_history_files(history_dir: &Path) -> Result<usize> {
    let mut valid = 0;
    for entry in fs::read_dir(history_dir).with_context(|| {
        format!(
            "cannot read Codex history directory {}",
            history_dir.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("md")
        {
            continue;
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("cannot read Codex history {}", path.display()))?;
        if session_from_header(&text).is_some() {
            valid += 1;
        }
    }
    Ok(valid)
}

fn session_from_header(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        line.strip_prefix("- Session: `")
            .and_then(|value| value.strip_suffix('`'))
            .filter(|value| !value.is_empty())
    })
}

fn history_belongs_to_session(text: &str, session_id: &str) -> bool {
    session_from_header(text) == Some(session_id)
}

fn latest_user_input_id(text: &str, session_id: &str) -> Option<String> {
    let prefix = format!("<!-- codex-event:{session_id}:");
    text.lines()
        .filter_map(|line| {
            line.strip_prefix(&prefix)
                .and_then(|value| value.strip_suffix(":user -->"))
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_latest_user_input_for_a_session() {
        let text = "# Codex conversation\n\n- Session: `session-a`\n\n<!-- codex-event:session-a:turn-1:user -->\n<!-- codex-event:session-a:turn-1:codex -->\n<!-- codex-event:session-a:turn-2:user -->\n";
        assert!(history_belongs_to_session(text, "session-a"));
        assert_eq!(
            latest_user_input_id(text, "session-a").as_deref(),
            Some("turn-2")
        );
        assert_eq!(latest_user_input_id(text, "session-b"), None);
        assert_eq!(safe_component("..thr:one///two.."), "thr-one-two");
    }
}
