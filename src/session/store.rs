use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::{Message, Session, TodoItem};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub todo_items: Vec<TodoItem>,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
}

pub fn sessions_dir() -> Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join("sessions"))
}

pub fn new_session_id() -> String {
    format!(
        "{}-{}",
        Utc::now().format("%Y%m%d%H%M%S%3f"),
        std::process::id()
    )
}

pub fn default_title(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("session")
        .to_string()
}

pub fn title_from_prompt(prompt: &str) -> String {
    const MAX_CHARS: usize = 24;
    let compact = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("session")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if compact.is_empty() {
        return "session".to_string();
    }

    let mut title: String = compact.chars().take(MAX_CHARS).collect();
    if compact.chars().count() > MAX_CHARS {
        title.push_str("...");
    }
    title
}

pub fn save(id: &str, title: &str, cwd: &Path, session: &Session) -> Result<()> {
    let dir = sessions_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = session_path_in(&dir, id);
    let now = Utc::now();
    let created_at = load(id).map(|s| s.created_at).unwrap_or(now);
    let stored = StoredSession {
        id: id.to_string(),
        title: title.trim().to_string(),
        cwd: cwd.display().to_string(),
        created_at,
        updated_at: now,
        messages: session.messages.clone(),
        todo_items: session.todo_items.clone(),
    };
    let raw = serde_json::to_string_pretty(&stored)?;
    std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn load(id: &str) -> Result<StoredSession> {
    let path = session_path(id)?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

pub fn delete(id: &str) -> Result<()> {
    let path = session_path(id)?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("delete {}", path.display()))?;
    }
    Ok(())
}

pub fn rename(id: &str, title: &str) -> Result<()> {
    let mut stored = load(id)?;
    stored.title = title.trim().to_string();
    stored.updated_at = Utc::now();
    write_stored(&stored)
}

pub fn fork(source_id: &str, cwd: &Path, title: Option<&str>) -> Result<StoredSession> {
    let source = load(source_id)?;
    let now = Utc::now();
    let forked = StoredSession {
        id: new_session_id(),
        title: title
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("{} (fork)", source.title)),
        cwd: cwd.display().to_string(),
        created_at: now,
        updated_at: now,
        messages: source.messages,
        todo_items: source.todo_items,
    };
    write_stored(&forked)?;
    Ok(forked)
}

pub fn list(scope_cwd: Option<&Path>) -> Result<Vec<SessionSummary>> {
    let dir = sessions_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let scope = scope_cwd.map(|p| normalize_path_string(&p.display().to_string()));
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(stored) = serde_json::from_str::<StoredSession>(&raw) else {
            continue;
        };
        if let Some(scope) = &scope {
            if normalize_path_string(&stored.cwd) != *scope {
                continue;
            }
        }
        out.push(SessionSummary {
            id: stored.id,
            title: stored.title,
            cwd: stored.cwd,
            updated_at: stored.updated_at,
            message_count: stored.messages.len(),
        });
    }
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(out)
}

fn write_stored(stored: &StoredSession) -> Result<()> {
    let dir = sessions_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = session_path_in(&dir, &stored.id);
    let raw = serde_json::to_string_pretty(stored)?;
    std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn session_path(id: &str) -> Result<PathBuf> {
    Ok(session_path_in(&sessions_dir()?, id))
}

fn session_path_in(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{}.json", sanitize_id(id)))
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn normalize_path_string(s: &str) -> String {
    s.replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}
