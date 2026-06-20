use crate::types::{ToolDefinition, ToolOutput};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use glob::glob;
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::timeout;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
	Allow,
	AllowAll,
	Deny,
}

#[async_trait]
pub trait PermissionPrompter: Send + Sync {
	async fn confirm(&self, request: PermissionRequest) -> Result<PermissionDecision>;
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
	pub tool_name: String,
	pub summary: String,
	pub diff: Option<String>,
}

#[derive(Clone)]
pub struct ToolContext {
	pub cwd: PathBuf,
	pub dangerously_skip_permissions: bool,
	pub allow_all: bool,
	pub prompter: Arc<dyn PermissionPrompter>,
}

impl ToolContext {
	pub fn new(cwd: PathBuf, dangerously_skip_permissions: bool, prompter: Arc<dyn PermissionPrompter>) -> Self {
		Self {
			cwd,
			dangerously_skip_permissions,
			allow_all: false,
			prompter,
		}
	}

	pub async fn confirm(&mut self, request: PermissionRequest) -> Result<()> {
		if self.dangerously_skip_permissions || self.allow_all {
			return Ok(());
		}
		match self.prompter.confirm(request).await? {
			PermissionDecision::Allow => Ok(()),
			PermissionDecision::AllowAll => {
				self.allow_all = true;
				Ok(())
			}
			PermissionDecision::Deny => anyhow::bail!("用户拒绝执行该工具"),
		}
	}
}

#[async_trait]
pub trait Tool: Send + Sync {
	fn name(&self) -> &'static str;
	fn description(&self) -> &'static str;
	fn schema(&self) -> Value;
	async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput>;

	fn definition(&self) -> ToolDefinition {
		ToolDefinition {
			name: self.name().to_string(),
			description: self.description().to_string(),
			input_schema: self.schema(),
		}
	}
}

pub struct ToolRegistry {
	tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
	pub fn builtin() -> Self {
		let mut registry = Self { tools: HashMap::new() };
		registry.register(ReadFileTool);
		registry.register(WriteFileTool);
		registry.register(EditFileTool);
		registry.register(BashTool);
		registry.register(GlobSearchTool);
		registry.register(GrepSearchTool);
		registry.register(ListDirTool);
		registry
	}

	pub fn register<T: Tool + 'static>(&mut self, tool: T) {
		self.tools.insert(tool.name().to_string(), Arc::new(tool));
	}

	pub fn definitions(&self) -> Vec<ToolDefinition> {
		let mut definitions = self.tools.values().map(|tool| tool.definition()).collect::<Vec<_>>();
		definitions.sort_by(|a, b| a.name.cmp(&b.name));
		definitions
	}

	pub async fn execute(&self, name: &str, args: Value, context: &mut ToolContext) -> ToolOutput {
		let Some(tool) = self.tools.get(name) else {
			return ToolOutput::error(format!("未知工具: {name}"));
		};
		match tool.execute(args, context).await {
			Ok(output) => output,
			Err(error) => ToolOutput::error(error.to_string()),
		}
	}
}

fn string_arg(args: &Value, key: &str) -> Result<String> {
	args.get(key)
		.and_then(Value::as_str)
		.map(ToString::to_string)
		.ok_or_else(|| anyhow!("缺少字符串参数: {key}"))
}

fn optional_u64_arg(args: &Value, key: &str, default: u64) -> u64 {
	args.get(key).and_then(Value::as_u64).unwrap_or(default)
}

fn resolve_path(cwd: &Path, raw: &str) -> Result<PathBuf> {
	let path = Path::new(raw);
	let joined = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
	let normalized = normalize_path(&joined);
	let normalized_cwd = normalize_path(cwd);
	anyhow::ensure!(normalized.starts_with(&normalized_cwd), "路径必须位于当前工作目录内: {raw}");
	Ok(normalized)
}

fn normalize_path(path: &Path) -> PathBuf {
	let mut output = PathBuf::new();
	for component in path.components() {
		match component {
			std::path::Component::CurDir => {}
			std::path::Component::ParentDir => {
				output.pop();
			}
			other => output.push(other.as_os_str()),
		}
	}
	output
}

fn diff_text(old: &str, new: &str) -> String {
	let diff = TextDiff::from_lines(old, new);
	let mut rendered = String::new();
	for change in diff.iter_all_changes() {
		let sign = match change.tag() {
			ChangeTag::Delete => "-",
			ChangeTag::Insert => "+",
			ChangeTag::Equal => " ",
		};
		rendered.push_str(sign);
		rendered.push_str(change.value());
	}
	rendered
}

struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
	fn name(&self) -> &'static str { "read_file" }
	fn description(&self) -> &'static str { "读取工作目录内的文本文件" }
	fn schema(&self) -> Value {
		json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
	}
	async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
		let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
		let content = tokio::fs::read_to_string(&path).await.with_context(|| format!("读取文件失败: {}", path.display()))?;
		Ok(ToolOutput::ok(content))
	}
}

struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
	fn name(&self) -> &'static str { "write_file" }
	fn description(&self) -> &'static str { "写入工作目录内的文本文件，执行前展示 diff 并请求确认" }
	fn schema(&self) -> Value {
		json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})
	}
	async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
		let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
		let content = string_arg(&args, "content")?;
		let old = tokio::fs::read_to_string(&path).await.unwrap_or_default();
		let diff = diff_text(&old, &content);
		context.confirm(PermissionRequest {
			tool_name: self.name().to_string(),
			summary: format!("写入文件 {}", path.display()),
			diff: Some(diff),
		}).await?;
		if let Some(parent) = path.parent() {
			tokio::fs::create_dir_all(parent).await?;
		}
		tokio::fs::write(&path, content).await.with_context(|| format!("写入文件失败: {}", path.display()))?;
		Ok(ToolOutput::ok(format!("已写入 {}", path.display())))
	}
}

struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
	fn name(&self) -> &'static str { "edit_file" }
	fn description(&self) -> &'static str { "对文本文件执行精确字符串替换，执行前展示 diff 并请求确认" }
	fn schema(&self) -> Value {
		json!({"type":"object","properties":{"path":{"type":"string"},"old_str":{"type":"string"},"new_str":{"type":"string"}},"required":["path","old_str","new_str"]})
	}
	async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
		let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
		let old_str = string_arg(&args, "old_str")?;
		let new_str = string_arg(&args, "new_str")?;
		let old = tokio::fs::read_to_string(&path).await.with_context(|| format!("读取文件失败: {}", path.display()))?;
		let matches = old.matches(&old_str).count();
		anyhow::ensure!(matches == 1, "old_str 必须精确出现一次，当前出现 {matches} 次");
		let new = old.replacen(&old_str, &new_str, 1);
		context.confirm(PermissionRequest {
			tool_name: self.name().to_string(),
			summary: format!("编辑文件 {}", path.display()),
			diff: Some(diff_text(&old, &new)),
		}).await?;
		tokio::fs::write(&path, new).await.with_context(|| format!("写入文件失败: {}", path.display()))?;
		Ok(ToolOutput::ok(format!("已编辑 {}", path.display())))
	}
}

struct BashTool;

#[async_trait]
impl Tool for BashTool {
	fn name(&self) -> &'static str { "bash" }
	fn description(&self) -> &'static str { "在当前工作目录执行 shell 命令，执行前请求确认" }
	fn schema(&self) -> Value {
		json!({"type":"object","properties":{"command":{"type":"string"},"timeout":{"type":"integer","description":"超时时间，单位秒，默认 30"}},"required":["command"]})
	}
	async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
		let command = string_arg(&args, "command")?;
		let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
		context.confirm(PermissionRequest {
			tool_name: self.name().to_string(),
			summary: format!("执行命令: {command}"),
			diff: None,
		}).await?;
		let started = Instant::now();
		let output = timeout(
			Duration::from_secs(timeout_secs),
			Command::new("sh").arg("-c").arg(&command).current_dir(&context.cwd).output(),
		)
		.await
		.map_err(|_| anyhow!("命令超时，已终止: {command}"))??;
		let elapsed = started.elapsed().as_secs_f32();
		let mut rendered = format!("exit: {} ({elapsed:.1}s)\n", output.status);
		if !output.stdout.is_empty() {
			rendered.push_str("stdout:\n");
			rendered.push_str(&String::from_utf8_lossy(&output.stdout));
		}
		if !output.stderr.is_empty() {
			rendered.push_str("\nstderr:\n");
			rendered.push_str(&String::from_utf8_lossy(&output.stderr));
		}
		Ok(ToolOutput { content: rendered, is_error: !output.status.success() })
	}
}

struct GlobSearchTool;

#[async_trait]
impl Tool for GlobSearchTool {
	fn name(&self) -> &'static str { "glob_search" }
	fn description(&self) -> &'static str { "按 glob 模式查找工作目录内文件" }
	fn schema(&self) -> Value {
		json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]})
	}
	async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
		let pattern = string_arg(&args, "pattern")?;
		let absolute_pattern = context.cwd.join(pattern).to_string_lossy().to_string();
		let mut matches = Vec::new();
		for entry in glob(&absolute_pattern)? {
			let path = entry?;
			if path.is_file() {
				matches.push(path.strip_prefix(&context.cwd).unwrap_or(&path).display().to_string());
			}
			if matches.len() >= 200 {
				break;
			}
		}
		matches.sort();
		Ok(ToolOutput::ok(matches.join("\n")))
	}
}

struct GrepSearchTool;

#[async_trait]
impl Tool for GrepSearchTool {
	fn name(&self) -> &'static str { "grep_search" }
	fn description(&self) -> &'static str { "在工作目录内递归搜索文本片段" }
	fn schema(&self) -> Value {
		json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","description":"可选，默认当前目录"}},"required":["pattern"]})
	}
	async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
		let pattern = string_arg(&args, "pattern")?;
		let root = match args.get("path").and_then(Value::as_str) {
			Some(path) => resolve_path(&context.cwd, path)?,
			None => context.cwd.clone(),
		};
		let mut lines = Vec::new();
		for entry in WalkDir::new(root).into_iter().filter_map(Result::ok).filter(|entry| entry.file_type().is_file()) {
			let path = entry.path();
			if path.components().any(|component| component.as_os_str() == "target" || component.as_os_str() == ".git") {
				continue;
			}
			let Ok(content) = std::fs::read_to_string(path) else { continue };
			for (line_number, line) in content.lines().enumerate() {
				if line.contains(&pattern) {
					let rel = path.strip_prefix(&context.cwd).unwrap_or(path).display();
					lines.push(format!("{}:{}:{}", rel, line_number + 1, line));
					if lines.len() >= 200 {
						return Ok(ToolOutput::ok(lines.join("\n")));
					}
				}
			}
		}
		Ok(ToolOutput::ok(lines.join("\n")))
	}
}

struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
	fn name(&self) -> &'static str { "list_dir" }
	fn description(&self) -> &'static str { "列出工作目录内某个目录的直接子项" }
	fn schema(&self) -> Value {
		json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
	}
	async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
		let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
		let mut entries = tokio::fs::read_dir(&path).await.with_context(|| format!("读取目录失败: {}", path.display()))?;
		let mut names = Vec::new();
		while let Some(entry) = entries.next_entry().await? {
			let file_type = entry.file_type().await?;
			let mut name = entry.file_name().to_string_lossy().to_string();
			if file_type.is_dir() {
				name.push('/');
			}
			names.push(name);
		}
		names.sort();
		Ok(ToolOutput::ok(names.join("\n")))
	}
}

pub struct AlwaysAllowPrompter;

#[async_trait]
impl PermissionPrompter for AlwaysAllowPrompter {
	async fn confirm(&self, _request: PermissionRequest) -> Result<PermissionDecision> {
		Ok(PermissionDecision::Allow)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tempfile::tempdir;

	fn context(dir: &Path) -> ToolContext {
		ToolContext::new(dir.to_path_buf(), true, Arc::new(AlwaysAllowPrompter))
	}

	#[tokio::test]
	async fn reads_and_edits_file() {
		let dir = tempdir().unwrap();
		let registry = ToolRegistry::builtin();
		let mut ctx = context(dir.path());
		registry.execute("write_file", json!({"path":"a.txt","content":"hello world"}), &mut ctx).await;
		let output = registry.execute("edit_file", json!({"path":"a.txt","old_str":"world","new_str":"yunzhi"}), &mut ctx).await;
		assert!(!output.is_error, "{}", output.content);
		let output = registry.execute("read_file", json!({"path":"a.txt"}), &mut ctx).await;
		assert_eq!(output.content, "hello yunzhi");
	}

	#[tokio::test]
	async fn rejects_ambiguous_edit() {
		let dir = tempdir().unwrap();
		let registry = ToolRegistry::builtin();
		let mut ctx = context(dir.path());
		registry.execute("write_file", json!({"path":"a.txt","content":"x x"}), &mut ctx).await;
		let output = registry.execute("edit_file", json!({"path":"a.txt","old_str":"x","new_str":"y"}), &mut ctx).await;
		assert!(output.is_error);
	}
}
