use crate::{config::LoadedConfig, model::AgentContext};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{env, fs, path::Path};

pub const SNAPSHOT_PATH: &str = "context/agent-history.md";
pub const CODEX: &str = "codex";
pub const CLAUDE: &str = "claude";

pub struct ResolvedAgentContext {
    pub metadata: AgentContext,
    bytes: Vec<u8>,
}

impl ResolvedAgentContext {
    pub fn write_snapshot(&self, run_dir: &Path) -> Result<()> {
        let destination = run_dir.join(SNAPSHOT_PATH);
        let parent = destination
            .parent()
            .context("agent context snapshot has no parent directory")?;
        fs::create_dir_all(parent)?;
        fs::write(&destination, &self.bytes)
            .with_context(|| format!("cannot write {}", destination.display()))
    }
}

/// An agent session visible to this process, identified by the agent that launched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub agent: &'static str,
    pub id: String,
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

/// Sessions exported by Codex and Claude Code. Both may be present when one agent was
/// started from the other's shell; resolution then requires exactly one history match.
pub fn current_sessions() -> Result<Vec<AgentSession>> {
    let mut sessions = Vec::new();
    let codex = non_empty_env("CODEX_SESSION_ID");
    let thread = non_empty_env("CODEX_THREAD_ID");
    if let (Some(session), Some(thread)) = (&codex, &thread)
        && session != thread
    {
        bail!(
            "CODEX_SESSION_ID and CODEX_THREAD_ID disagree; refusing to attach ambiguous agent context"
        );
    }
    if let Some(id) = codex.or(thread) {
        sessions.push(AgentSession {
            agent: CODEX,
            id: safe_component(&id),
        });
    }
    if let Some(id) = non_empty_env("CLAUDE_CODE_SESSION_ID") {
        sessions.push(AgentSession {
            agent: CLAUDE,
            id: safe_component(&id),
        });
    }
    if sessions.is_empty() {
        bail!(
            "agent context is required, but neither CODEX_SESSION_ID/CODEX_THREAD_ID nor CLAUDE_CODE_SESSION_ID is set; run the benchmark from Codex or Claude Code"
        );
    }
    Ok(sessions)
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

pub fn resolve(config: &LoadedConfig) -> Result<Option<ResolvedAgentContext>> {
    let Some(history_dir) = config.agent_history_dir() else {
        return Ok(None);
    };
    let sessions = current_sessions()?;
    let source_repo = config
        .source_repo()
        .canonicalize()
        .context("cannot resolve source repository for agent context")?;
    let history_dir = history_dir.canonicalize().with_context(|| {
        format!(
            "cannot resolve agent history directory {}",
            history_dir.display()
        )
    })?;
    if !history_dir.starts_with(&source_repo) {
        bail!(
            "agent history directory must remain inside source repository {}",
            source_repo.display()
        );
    }

    let mut matches = Vec::new();
    for (path, bytes) in history_files(&history_dir)? {
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("agent history is not UTF-8: {}", path.display()))?;
        for session in &sessions {
            if history_belongs_to_session(text, session) {
                let input_id = latest_user_input_id(text, session);
                matches.push((path.clone(), bytes.clone(), session.clone(), input_id));
            }
        }
    }
    let described = sessions
        .iter()
        .map(|session| format!("{} `{}`", session.agent, session.id))
        .collect::<Vec<_>>()
        .join(", ");
    if matches.is_empty() {
        bail!(
            "no agent history file in {} belongs to current session {described}; ensure the UserPromptSubmit hook was active before starting this session",
            history_dir.display(),
        );
    }
    if matches.len() > 1 {
        bail!(
            "multiple agent history files in {} belong to current session {described}",
            history_dir.display(),
        );
    }
    let (path, bytes, session, input_id) = matches.pop().expect("one history match was checked");
    let input_id = input_id.with_context(|| {
        format!(
            "agent history {} has no User input marker for current {} session `{}`",
            path.display(),
            session.agent,
            session.id
        )
    })?;
    let relative = path.strip_prefix(&source_repo).with_context(|| {
        format!(
            "agent history {} is outside source repository {}",
            path.display(),
            source_repo.display()
        )
    })?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    Ok(Some(ResolvedAgentContext {
        metadata: AgentContext {
            agent: session.agent.into(),
            history_path: relative.display().to_string(),
            session_id: session.id,
            input_id,
            snapshot_path: SNAPSHOT_PATH.into(),
            sha256,
        },
        bytes,
    }))
}

fn history_files(history_dir: &Path) -> Result<Vec<(std::path::PathBuf, Vec<u8>)>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(history_dir).with_context(|| {
        format!(
            "cannot read agent history directory {}",
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
            .with_context(|| format!("cannot read agent history {}", path.display()))?;
        files.push((path, bytes));
    }
    Ok(files)
}

pub fn valid_history_files(history_dir: &Path) -> Result<usize> {
    let mut valid = 0;
    for (path, bytes) in history_files(history_dir)? {
        let text = String::from_utf8(bytes)
            .with_context(|| format!("agent history is not UTF-8: {}", path.display()))?;
        if header_value(&text, "Session").is_some() {
            valid += 1;
        }
    }
    Ok(valid)
}

fn header_value<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("- {name}: `");
    text.lines().take(16).find_map(|line| {
        line.strip_prefix(&prefix)
            .and_then(|value| value.strip_suffix('`'))
            .filter(|value| !value.is_empty())
    })
}

/// Files written before the shared hook have no `Agent` header and are Codex histories.
fn history_belongs_to_session(text: &str, session: &AgentSession) -> bool {
    header_value(text, "Session") == Some(session.id.as_str())
        && header_value(text, "Agent").unwrap_or(CODEX) == session.agent
}

fn latest_user_input_id(text: &str, session: &AgentSession) -> Option<String> {
    let current = format!("<!-- agent-event:{}:{}:", session.agent, session.id);
    let legacy = (session.agent == CODEX).then(|| format!("<!-- codex-event:{}:", session.id));
    text.lines()
        .filter_map(|line| {
            line.strip_prefix(&current)
                .or_else(|| {
                    legacy
                        .as_deref()
                        .and_then(|prefix| line.strip_prefix(prefix))
                })
                .and_then(|value| value.strip_suffix(":user -->"))
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(agent: &'static str, id: &str) -> AgentSession {
        AgentSession {
            agent,
            id: id.into(),
        }
    }

    #[test]
    fn selects_the_latest_user_input_for_a_legacy_codex_session() {
        let text = "# Codex conversation\n\n- Session: `session-a`\n\n<!-- codex-event:session-a:turn-1:user -->\n<!-- codex-event:session-a:turn-1:codex -->\n<!-- codex-event:session-a:turn-2:user -->\n";
        let codex = session(CODEX, "session-a");
        assert!(history_belongs_to_session(text, &codex));
        assert!(!history_belongs_to_session(
            text,
            &session(CLAUDE, "session-a")
        ));
        assert_eq!(
            latest_user_input_id(text, &codex).as_deref(),
            Some("turn-2")
        );
        assert_eq!(
            latest_user_input_id(text, &session(CODEX, "session-b")),
            None
        );
        assert_eq!(safe_component("..thr:one///two.."), "thr-one-two");
    }

    #[test]
    fn selects_the_latest_user_input_for_an_agent_session() {
        let text = "# Agent conversation\n\n- Agent: `claude`\n- Session: `s1`\n\n<!-- agent-event:claude:s1:p1:user -->\n<!-- agent-event:claude:s1:p1:claude -->\n<!-- agent-event:claude:s1:p2:user -->\n";
        let claude = session(CLAUDE, "s1");
        assert!(history_belongs_to_session(text, &claude));
        assert!(!history_belongs_to_session(text, &session(CODEX, "s1")));
        assert_eq!(latest_user_input_id(text, &claude).as_deref(), Some("p2"));
    }
}
