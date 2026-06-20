use crate::types::Message;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct UsageMetrics {
    pub request_count: u64,
    pub tool_call_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    pub estimated_cost_usd: f64,
}

impl UsageMetrics {
    pub fn total_tokens(self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn add_turn(&mut self, turn: TurnMetrics) {
        self.request_count += turn.request_count;
        self.tool_call_count += turn.tool_call_count;
        self.input_tokens += turn.input_tokens;
        self.output_tokens += turn.output_tokens;
        self.elapsed_ms += turn.elapsed_ms;
        self.estimated_cost_usd += turn.estimated_cost_usd;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TurnMetrics {
    pub request_count: u64,
    pub tool_call_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    pub estimated_cost_usd: f64,
}

impl TurnMetrics {
    pub fn total_tokens(self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolAuditRecord {
    pub timestamp_unix: u64,
    pub call_id: String,
    pub tool_name: String,
    pub input: Value,
    pub output_preview: String,
    pub is_error: bool,
    pub elapsed_ms: u64,
}

pub fn audit_log_path(cwd: &Path) -> PathBuf {
    cwd.join(".yunzhi").join("audit").join("tools.jsonl")
}

pub fn append_tool_audit(cwd: &Path, record: &ToolAuditRecord) -> Result<PathBuf> {
    let path = audit_log_path(cwd);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建审计日志目录失败: {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("打开审计日志失败: {}", path.display()))?;
    writeln!(file, "{}", serde_json::to_string(record)?)
        .with_context(|| format!("写入审计日志失败: {}", path.display()))?;
    Ok(path)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn estimate_tokens(text: &str) -> u64 {
    text.chars().count().div_ceil(4) as u64
}

pub fn estimate_request_tokens(system: Option<&str>, messages: &[Message]) -> u64 {
    system.map(estimate_tokens).unwrap_or_default()
        + messages
            .iter()
            .map(|message| estimate_tokens(&message.text()))
            .sum::<u64>()
}

pub fn estimate_cost_usd(model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    let (input_per_million, output_per_million) = pricing_per_million(model);
    (input_tokens as f64 / 1_000_000.0 * input_per_million)
        + (output_tokens as f64 / 1_000_000.0 * output_per_million)
}

pub fn truncate_preview(text: &str, max_chars: usize) -> String {
    let mut preview = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        preview.push_str("...[truncated]");
    }
    preview
}

fn pricing_per_million(model: &str) -> (f64, f64) {
    let normalized = model.to_ascii_lowercase();
    if normalized.contains("gemini") && normalized.contains("flash") {
        (0.30, 2.50)
    } else if normalized.contains("gemini") && normalized.contains("pro") {
        (1.25, 10.0)
    } else if normalized.contains("claude") && normalized.contains("opus") {
        (15.0, 75.0)
    } else if normalized.contains("claude") && normalized.contains("sonnet") {
        (3.0, 15.0)
    } else {
        (0.0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn estimates_tokens_by_characters() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn appends_tool_audit_jsonl() {
        let dir = tempdir().unwrap();
        let record = ToolAuditRecord {
            timestamp_unix: 1,
            call_id: "call-1".to_string(),
            tool_name: "read_file".to_string(),
            input: serde_json::json!({"path":"README.md"}),
            output_preview: "ok".to_string(),
            is_error: false,
            elapsed_ms: 7,
        };
        let path = append_tool_audit(dir.path(), &record).unwrap();
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(raw.contains("read_file"));
        assert!(raw.ends_with('\n'));
    }
}
