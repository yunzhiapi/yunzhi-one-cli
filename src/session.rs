use crate::observability::UsageMetrics;
use crate::types::Message;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedSession {
    pub id: String,
    pub created_unix: u64,
    pub updated_unix: u64,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub usage: UsageMetrics,
    #[serde(default)]
    pub checkpoints: Vec<SessionCheckpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionCheckpoint {
    pub id: String,
    pub created_unix: u64,
    pub snapshot_dir: String,
    pub note: Option<String>,
}

pub fn sessions_dir(cwd: &Path) -> PathBuf {
    cwd.join(".yunzhi").join("sessions")
}

pub fn session_path(cwd: &Path, id: &str) -> PathBuf {
    sessions_dir(cwd).join(format!("{id}.json"))
}

pub fn load_session(cwd: &Path, id: &str) -> Result<SavedSession> {
    let path = session_path(cwd, id);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("读取会话失败: {}", path.display()))?;
    toml_safe_json_from_str(&raw).with_context(|| format!("解析会话失败: {}", path.display()))
}

pub fn save_session(cwd: &Path, session: &SavedSession) -> Result<PathBuf> {
    let dir = sessions_dir(cwd);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建会话目录失败: {}", dir.display()))?;
    let path = session_path(cwd, &session.id);
    let raw = serde_json::to_string_pretty(session).context("序列化会话失败")?;
    std::fs::write(&path, raw).with_context(|| format!("写入会话失败: {}", path.display()))?;
    Ok(path)
}

pub fn list_sessions(cwd: &Path) -> Result<Vec<String>> {
    let dir = sessions_dir(cwd);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            if let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) {
                ids.push(id.to_string());
            }
        }
    }
    ids.sort();
    Ok(ids)
}

pub fn create_checkpoint(
    cwd: &Path,
    session: &mut SavedSession,
    note: Option<String>,
) -> Result<String> {
    let id = format!("cp-{}", now_unix());
    let snapshot_dir = sessions_dir(cwd)
        .join("snapshots")
        .join(&session.id)
        .join(&id);
    copy_snapshot(cwd, &snapshot_dir)?;
    session.checkpoints.push(SessionCheckpoint {
        id: id.clone(),
        created_unix: now_unix(),
        snapshot_dir: snapshot_dir
            .strip_prefix(cwd)
            .unwrap_or(&snapshot_dir)
            .display()
            .to_string(),
        note,
    });
    session.updated_unix = now_unix();
    save_session(cwd, session)?;
    Ok(id)
}

pub fn rollback_checkpoint(cwd: &Path, session: &SavedSession, checkpoint_id: &str) -> Result<()> {
    let checkpoint = session
        .checkpoints
        .iter()
        .find(|checkpoint| checkpoint.id == checkpoint_id)
        .with_context(|| format!("未找到 checkpoint: {checkpoint_id}"))?;
    let snapshot = cwd.join(&checkpoint.snapshot_dir);
    clear_workspace_files(cwd)?;
    restore_snapshot(&snapshot, cwd)
}

pub fn new_session(id: String, messages: Vec<Message>) -> SavedSession {
    let now = now_unix();
    SavedSession {
        id,
        created_unix: now,
        updated_unix: now,
        messages,
        usage: UsageMetrics::default(),
        checkpoints: Vec::new(),
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn toml_safe_json_from_str(raw: &str) -> Result<SavedSession> {
    Ok(serde_json::from_str(raw)?)
}

fn copy_snapshot(cwd: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in ignore::WalkBuilder::new(cwd)
        .hidden(false)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .build()
    {
        let entry = entry?;
        let path = entry.path();
        if path == cwd || is_session_path(cwd, path) || is_default_excluded_path(cwd, path) {
            continue;
        }
        let file_type = entry.file_type();
        if !file_type.is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let rel = path.strip_prefix(cwd).unwrap_or(path);
        let target = destination.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(path, target)?;
    }
    Ok(())
}

fn clear_workspace_files(cwd: &Path) -> Result<()> {
    for entry in ignore::WalkBuilder::new(cwd)
        .hidden(false)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .build()
    {
        let entry = entry?;
        let path = entry.path();
        if path == cwd || is_session_path(cwd, path) || is_default_excluded_path(cwd, path) {
            continue;
        }
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn is_session_path(cwd: &Path, path: &Path) -> bool {
    path.starts_with(sessions_dir(cwd))
}

fn is_default_excluded_path(cwd: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(cwd) else {
        return false;
    };
    rel.components().any(|component| {
        let part = component.as_os_str();
        part == ".git" || part == "target" || part == "node_modules"
    })
}

fn restore_snapshot(snapshot: &Path, cwd: &Path) -> Result<()> {
    for entry in ignore::WalkBuilder::new(snapshot)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .parents(false)
        .build()
    {
        let entry = entry?;
        let path = entry.path();
        if path == snapshot {
            continue;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let rel = path.strip_prefix(snapshot).unwrap_or(path);
        let target = cwd.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(path, target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use tempfile::tempdir;

    #[test]
    fn saves_loads_and_rolls_back_checkpoint() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        let mut session = new_session("demo".to_string(), vec![Message::user("hello")]);
        save_session(dir.path(), &session).unwrap();
        let checkpoint =
            create_checkpoint(dir.path(), &mut session, Some("before".to_string())).unwrap();
        std::fs::write(dir.path().join("a.txt"), "two").unwrap();
        rollback_checkpoint(dir.path(), &session, &checkpoint).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one"
        );
        assert_eq!(load_session(dir.path(), "demo").unwrap().messages.len(), 1);
        assert_eq!(list_sessions(dir.path()).unwrap(), vec!["demo"]);
    }
}
