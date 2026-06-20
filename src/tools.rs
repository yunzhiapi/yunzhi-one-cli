use crate::extensions::{
    call_mcp_tool, get_mcp_prompt, list_mcp_prompts, list_mcp_resources, load_mcp_servers,
    read_mcp_resource, read_skill, skills_index,
};
use crate::llm::ChatCompletionsClient;
use crate::types::{ToolDefinition, ToolOutput};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use glob::Pattern;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    AllowAll,
    Partial(Vec<usize>),
    Deny,
}

#[async_trait]
pub trait PermissionPrompter: Send + Sync {
    async fn confirm(&self, request: PermissionRequest) -> Result<PermissionDecision>;
    async fn ask_user(&self, request: UserQuestionRequest) -> Result<String>;
    async fn choose_option(&self, request: UserChoiceRequest) -> Result<UserChoiceResponse>;
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub summary: String,
    pub diff: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UserQuestionRequest {
    pub question: String,
    pub context: Option<String>,
    pub default_answer: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UserChoiceRequest {
    pub question: String,
    pub context: Option<String>,
    pub options: Vec<String>,
    pub allow_custom: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserChoiceResponse {
    pub answer: String,
    pub index: Option<usize>,
    pub custom: bool,
}

#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub api_key: String,
    pub dangerously_skip_permissions: bool,
    pub allow_all: bool,
    pub prompter: Arc<dyn PermissionPrompter>,
    pub auto_approve_safe: bool,
    todos: Vec<TodoItem>,
    next_todo_id: u64,
}

impl ToolContext {
    pub fn new(
        cwd: PathBuf,
        api_key: String,
        dangerously_skip_permissions: bool,
        prompter: Arc<dyn PermissionPrompter>,
        auto_approve_safe: bool,
    ) -> Self {
        Self {
            cwd,
            api_key,
            dangerously_skip_permissions,
            allow_all: false,
            prompter,
            auto_approve_safe,
            todos: Vec::new(),
            next_todo_id: 1,
        }
    }

    pub async fn confirm(&mut self, request: PermissionRequest) -> Result<PermissionDecision> {
        if self.dangerously_skip_permissions || self.allow_all {
            return Ok(PermissionDecision::Allow);
        }
        if self.auto_approve_safe && is_safe_operation(&request.tool_name) {
            return Ok(PermissionDecision::Allow);
        }
        match self.prompter.confirm(request).await? {
            PermissionDecision::Allow => Ok(PermissionDecision::Allow),
            PermissionDecision::AllowAll => {
                self.allow_all = true;
                Ok(PermissionDecision::AllowAll)
            }
            PermissionDecision::Partial(hunks) => Ok(PermissionDecision::Partial(hunks)),
            PermissionDecision::Deny => anyhow::bail!("用户拒绝执行该工具"),
        }
    }
}

fn is_safe_operation(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "read_file"
            | "write_file"
            | "edit_file"
            | "append_file"
            | "create_dir"
            | "file_info"
            | "list_dir"
            | "glob_search"
            | "grep_search"
            | "manage_todos"
            | "ask_user"
            | "choose_option"
            | "list_models"
            | "list_skills"
            | "read_skill"
            | "list_mcp_servers"
            | "mcp_resource"
            | "mcp_prompt"
            | "call_model"
            | "create_presentation"
            | "generate_image"
            | "write_document"
            | "write_table"
            | "office_document"
            | "ui_design"
            | "long_memory"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub id: u64,
    pub title: String,
    pub status: TodoStatus,
    pub notes: Option<String>,
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
        let mut registry = Self {
            tools: HashMap::new(),
        };
        registry.register(ReadFileTool);
        registry.register(WriteFileTool);
        registry.register(EditFileTool);
        registry.register(AppendFileTool);
        registry.register(CreateDirTool);
        registry.register(CopyPathTool);
        registry.register(MovePathTool);
        registry.register(DeletePathTool);
        registry.register(FileInfoTool);
        registry.register(BashTool);
        registry.register(GlobSearchTool);
        registry.register(GrepSearchTool);
        registry.register(CodeIndexTool);
        registry.register(GitManagerTool);
        registry.register(ListDirTool);
        registry.register(ListModelsTool);
        registry.register(ListSkillsTool);
        registry.register(ReadSkillTool);
        registry.register(ListMcpServersTool);
        registry.register(CallMcpTool);
        registry.register(McpResourceTool);
        registry.register(McpPromptTool);
        registry.register(CallModelTool);
        registry.register(AskUserTool);
        registry.register(ChooseOptionTool);
        registry.register(ExecuteCodeTool);
        registry.register(RunProgramTool);
        registry.register(TestLoopTool);
        registry.register(ManageTodosTool);
        registry.register(SystemControlTool);
        registry.register(CreatePresentationTool);
        registry.register(GenerateImageTool);
        registry.register(WriteDocumentTool);
        registry.register(WriteTableTool);
        registry.register(OfficeDocumentTool);
        registry.register(DiskManagerTool);
        registry.register(ComputerManagerTool);
        registry.register(WebSearchTool);
        registry.register(BrowserTool);
        registry.register(NetworkLogsTool);
        registry.register(ComputerInfoTool);
        registry.register(DatabaseManagerTool);
        registry.register(UiDesignTool);
        registry.register(LongMemoryTool);
        registry
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .tools
            .values()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        definitions
    }

    pub fn definitions_for(&self, names: &[&str]) -> Vec<ToolDefinition> {
        let mut definitions = names
            .iter()
            .filter_map(|name| self.tools.get(*name).map(|tool| tool.definition()))
            .collect::<Vec<_>>();
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

fn optional_string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(|value| value.to_string())
}

fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn array_of_strings_arg(args: &Value, key: &str) -> Result<Vec<String>> {
    let Some(values) = args.get(key).and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .ok_or_else(|| anyhow!("{key} 必须全部是字符串"))
        })
        .collect()
}

fn resolve_path(cwd: &Path, raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let normalized = normalize_path(&joined);
    let normalized_cwd = normalize_path(cwd);
    anyhow::ensure!(
        normalized.starts_with(&normalized_cwd),
        "路径必须位于当前工作目录内: {raw}"
    );
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

#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub index: usize,
    pub old_range: (usize, usize),
    pub new_range: (usize, usize),
    pub old_text: String,
    pub new_text: String,
    pub diff: String,
}

fn diff_hunks(old: &str, new: &str) -> Vec<DiffHunk> {
    let diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();
    for (index, group) in diff.grouped_ops(3).into_iter().enumerate() {
        let mut old_text = String::new();
        let mut new_text = String::new();
        let mut rendered = String::new();
        let mut old_start = usize::MAX;
        let mut old_end = 0usize;
        let mut new_start = usize::MAX;
        let mut new_end = 0usize;

        for op in group {
            for change in diff.iter_changes(&op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                rendered.push_str(sign);
                rendered.push_str(change.value());
                match change.tag() {
                    ChangeTag::Delete => {
                        old_start = old_start.min(change.old_index().unwrap_or(0));
                        old_end = old_end.max(change.old_index().unwrap_or(0) + 1);
                        old_text.push_str(change.value());
                    }
                    ChangeTag::Insert => {
                        new_start = new_start.min(change.new_index().unwrap_or(0));
                        new_end = new_end.max(change.new_index().unwrap_or(0) + 1);
                        new_text.push_str(change.value());
                    }
                    ChangeTag::Equal => {
                        if let Some(old_index) = change.old_index() {
                            old_start = old_start.min(old_index);
                            old_end = old_end.max(old_index + 1);
                        }
                        if let Some(new_index) = change.new_index() {
                            new_start = new_start.min(new_index);
                            new_end = new_end.max(new_index + 1);
                        }
                        old_text.push_str(change.value());
                        new_text.push_str(change.value());
                    }
                }
            }
        }

        if old_start == usize::MAX {
            old_start = old_end;
        }
        if new_start == usize::MAX {
            new_start = new_end;
        }
        hunks.push(DiffHunk {
            index: index + 1,
            old_range: (old_start, old_end),
            new_range: (new_start, new_end),
            old_text,
            new_text,
            diff: rendered,
        });
    }
    hunks
}

fn format_diff_hunks(hunks: &[DiffHunk]) -> String {
    hunks
        .iter()
        .map(|hunk| {
            format!(
                "@@ hunk {} -{},{} +{},{} @@\n{}",
                hunk.index,
                hunk.old_range.0 + 1,
                hunk.old_range.1.saturating_sub(hunk.old_range.0),
                hunk.new_range.0 + 1,
                hunk.new_range.1.saturating_sub(hunk.new_range.0),
                hunk.diff
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn apply_selected_hunks(old: &str, hunks: &[DiffHunk], selected: &[usize]) -> String {
    if selected.is_empty() {
        return old.to_string();
    }
    let selected = selected.iter().copied().collect::<HashSet<_>>();
    let mut output = String::new();
    let mut cursor = 0usize;
    for hunk in hunks {
        let start = old_line_to_byte(old, hunk.old_range.0);
        let end = old_line_to_byte(old, hunk.old_range.1);
        if cursor < start {
            output.push_str(&old[cursor..start]);
        }
        if selected.contains(&hunk.index) {
            output.push_str(&hunk.new_text);
        } else {
            output.push_str(&old[start..end]);
        }
        cursor = end;
    }
    output.push_str(&old[cursor..]);
    output
}

fn old_line_to_byte(text: &str, line_index: usize) -> usize {
    if line_index == 0 {
        return 0;
    }
    text.char_indices()
        .filter_map(|(index, ch)| (ch == '\n').then_some(index + 1))
        .nth(line_index - 1)
        .unwrap_or(text.len())
}

async fn confirm_text_write(
    context: &mut ToolContext,
    tool_name: &str,
    summary: String,
    old: &str,
    new: String,
) -> Result<String> {
    let hunks = diff_hunks(old, &new);
    if hunks.is_empty() {
        return Ok(new);
    }
    let decision = context
        .confirm(PermissionRequest {
            tool_name: tool_name.to_string(),
            summary,
            diff: Some(format_diff_hunks(&hunks)),
        })
        .await?;
    match decision {
        PermissionDecision::Partial(selected) => Ok(apply_selected_hunks(old, &hunks, &selected)),
        _ => Ok(new),
    }
}

const DEFAULT_IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".next",
    "dist",
    "build",
    ".cache",
    ".venv",
    "venv",
];
const MAX_INDEXED_FILE_BYTES: u64 = 1_000_000;

fn is_default_ignored_path(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy();
        DEFAULT_IGNORED_DIRS.iter().any(|ignored| value == *ignored)
    })
}

fn searchable_files(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(|entry| !is_default_ignored_path(entry.path()))
        .build()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !path.is_file() || is_default_ignored_path(path) {
            continue;
        }
        if !normalize_path(path).starts_with(&normalize_path(cwd)) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        if metadata.len() > MAX_INDEXED_FILE_BYTES {
            continue;
        }
        files.push(path.to_path_buf());
    }
    files
}

fn is_text_like(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(
            "rs" | "toml"
                | "md"
                | "txt"
                | "json"
                | "yaml"
                | "yml"
                | "js"
                | "ts"
                | "tsx"
                | "jsx"
                | "py"
                | "go"
                | "java"
                | "c"
                | "h"
                | "cpp"
                | "hpp"
                | "cs"
                | "html"
                | "css"
                | "scss"
                | "sql"
                | "sh"
        )
    )
}

#[derive(Debug, Clone)]
struct CodeSymbol {
    kind: &'static str,
    name: String,
    line: usize,
}

impl CodeSymbol {
    fn matches_query(&self, query: Option<&str>) -> bool {
        query.is_none_or(|query| self.name.to_lowercase().contains(query))
    }
}

fn extract_symbols(path: &Path, content: &str) -> Vec<CodeSymbol> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    let mut symbols = Vec::new();
    for (line_index, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        let candidates: &[(&str, &str)] = match extension {
            "rs" => &[
                ("fn", "function"),
                ("pub fn", "function"),
                ("struct", "type"),
                ("pub struct", "type"),
                ("enum", "type"),
                ("pub enum", "type"),
                ("trait", "trait"),
                ("impl", "impl"),
            ],
            "ts" | "tsx" | "js" | "jsx" => &[
                ("function", "function"),
                ("export function", "function"),
                ("class", "type"),
                ("export class", "type"),
                ("interface", "type"),
                ("export interface", "type"),
                ("const", "value"),
                ("export const", "value"),
            ],
            "py" => &[("def", "function"), ("class", "type")],
            "go" => &[("func", "function"), ("type", "type")],
            _ => &[],
        };
        for (prefix, kind) in candidates {
            if let Some(name) = symbol_name_after_prefix(trimmed, prefix) {
                symbols.push(CodeSymbol {
                    kind,
                    name,
                    line: line_index + 1,
                });
                break;
            }
        }
    }
    symbols
}

fn symbol_name_after_prefix(line: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(prefix)?.trim_start();
    let rest = rest.strip_prefix("async ").unwrap_or(rest).trim_start();
    let rest = rest.strip_prefix("unsafe ").unwrap_or(rest).trim_start();
    let name = rest
        .trim_start_matches(|ch: char| ch == '<' || ch.is_whitespace())
        .chars()
        .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
        .collect::<String>();
    (!name.is_empty()).then_some(name)
}

fn format_code_index_file(path: &str, symbols: &[CodeSymbol]) -> String {
    if symbols.is_empty() {
        return format!("file\t{path}");
    }
    let rendered_symbols = symbols
        .iter()
        .take(20)
        .map(|symbol| format!("{}:{}:{}", symbol.kind, symbol.name, symbol.line))
        .collect::<Vec<_>>()
        .join(", ");
    format!("file\t{path}\t{rendered_symbols}")
}

async fn write_text_file_with_confirmation(
    tool_name: &str,
    path: &Path,
    content: String,
    context: &mut ToolContext,
    summary: String,
) -> Result<ToolOutput> {
    let old = tokio::fs::read_to_string(path).await.unwrap_or_default();
    let content = confirm_text_write(context, tool_name, summary, &old, content).await?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, content)
        .await
        .with_context(|| format!("写入文件失败: {}", path.display()))?;
    Ok(ToolOutput::ok(format!("已写入 {}", path.display())))
}

async fn write_binary_file_with_confirmation(
    tool_name: &str,
    path: &Path,
    content: Vec<u8>,
    context: &mut ToolContext,
    summary: String,
) -> Result<ToolOutput> {
    let old = fs::read(path).await.unwrap_or_default();
    context
        .confirm(PermissionRequest {
            tool_name: tool_name.to_string(),
            summary,
            diff: Some(format!(
                "binary file: {} bytes -> {} bytes",
                old.len(),
                content.len()
            )),
        })
        .await?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, content)
        .await
        .with_context(|| format!("写入文件失败: {}", path.display()))?;
    Ok(ToolOutput::ok(format!("已写入 {}", path.display())))
}

struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &'static str {
        "读取工作目录内的文本文件"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("读取文件失败: {}", path.display()))?;
        Ok(ToolOutput::ok(content))
    }
}

struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn description(&self) -> &'static str {
        "写入工作目录内的文本文件，执行前展示 diff 并请求确认"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        let content = string_arg(&args, "content")?;
        let old = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        let content = confirm_text_write(
            context,
            self.name(),
            format!("写入文件 {}", path.display()),
            &old,
            content,
        )
        .await?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, content)
            .await
            .with_context(|| format!("写入文件失败: {}", path.display()))?;
        Ok(ToolOutput::ok(format!("已写入 {}", path.display())))
    }
}

struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }
    fn description(&self) -> &'static str {
        "对文本文件执行精确字符串替换，执行前展示 diff 并请求确认"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"old_str":{"type":"string"},"new_str":{"type":"string"}},"required":["path","old_str","new_str"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        let old_str = string_arg(&args, "old_str")?;
        let new_str = string_arg(&args, "new_str")?;
        let old = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("读取文件失败: {}", path.display()))?;
        let matches = old.matches(&old_str).count();
        anyhow::ensure!(
            matches == 1,
            "old_str 必须精确出现一次，当前出现 {matches} 次"
        );
        let new = old.replacen(&old_str, &new_str, 1);
        let new = confirm_text_write(
            context,
            self.name(),
            format!("编辑文件 {}", path.display()),
            &old,
            new,
        )
        .await?;
        tokio::fs::write(&path, new)
            .await
            .with_context(|| format!("写入文件失败: {}", path.display()))?;
        Ok(ToolOutput::ok(format!("已编辑 {}", path.display())))
    }
}

struct AppendFileTool;

#[async_trait]
impl Tool for AppendFileTool {
    fn name(&self) -> &'static str {
        "append_file"
    }
    fn description(&self) -> &'static str {
        "向工作目录内的文本文件末尾追加内容，执行前展示 diff 并请求确认"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        let content = string_arg(&args, "content")?;
        let old = fs::read_to_string(&path).await.unwrap_or_default();
        let mut new = old.clone();
        new.push_str(&content);
        let new = confirm_text_write(
            context,
            self.name(),
            format!("追加文件 {}", path.display()),
            &old,
            new,
        )
        .await?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&path, new)
            .await
            .with_context(|| format!("追加文件失败: {}", path.display()))?;
        Ok(ToolOutput::ok(format!("已追加 {}", path.display())))
    }
}

struct CreateDirTool;

#[async_trait]
impl Tool for CreateDirTool {
    fn name(&self) -> &'static str {
        "create_dir"
    }
    fn description(&self) -> &'static str {
        "在工作目录内创建目录，可递归创建父目录"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!("创建目录 {}", path.display()),
                diff: None,
            })
            .await?;
        fs::create_dir_all(&path)
            .await
            .with_context(|| format!("创建目录失败: {}", path.display()))?;
        Ok(ToolOutput::ok(format!("已创建目录 {}", path.display())))
    }
}

struct CopyPathTool;

#[async_trait]
impl Tool for CopyPathTool {
    fn name(&self) -> &'static str {
        "copy_path"
    }
    fn description(&self) -> &'static str {
        "复制工作目录内的文件或目录，执行前请求确认"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"source":{"type":"string"},"destination":{"type":"string"}},"required":["source","destination"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let source = resolve_path(&context.cwd, &string_arg(&args, "source")?)?;
        let destination = resolve_path(&context.cwd, &string_arg(&args, "destination")?)?;
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!("复制 {} 到 {}", source.display(), destination.display()),
                diff: None,
            })
            .await?;
        copy_path(&source, &destination).await?;
        Ok(ToolOutput::ok(format!(
            "已复制 {} 到 {}",
            source.display(),
            destination.display()
        )))
    }
}

struct MovePathTool;

#[async_trait]
impl Tool for MovePathTool {
    fn name(&self) -> &'static str {
        "move_path"
    }
    fn description(&self) -> &'static str {
        "移动或重命名工作目录内的文件或目录，执行前请求确认"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"source":{"type":"string"},"destination":{"type":"string"}},"required":["source","destination"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let source = resolve_path(&context.cwd, &string_arg(&args, "source")?)?;
        let destination = resolve_path(&context.cwd, &string_arg(&args, "destination")?)?;
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!("移动 {} 到 {}", source.display(), destination.display()),
                diff: None,
            })
            .await?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::rename(&source, &destination).await.with_context(|| {
            format!(
                "移动路径失败: {} -> {}",
                source.display(),
                destination.display()
            )
        })?;
        Ok(ToolOutput::ok(format!(
            "已移动 {} 到 {}",
            source.display(),
            destination.display()
        )))
    }
}

struct DeletePathTool;

#[async_trait]
impl Tool for DeletePathTool {
    fn name(&self) -> &'static str {
        "delete_path"
    }
    fn description(&self) -> &'static str {
        "删除工作目录内的文件或目录，目录删除需要 recursive=true，执行前请求确认"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"recursive":{"type":"boolean","description":"删除目录时必须为 true"}},"required":["path"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        let recursive = args
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!("删除路径 {}", path.display()),
                diff: None,
            })
            .await?;
        let metadata = fs::metadata(&path)
            .await
            .with_context(|| format!("读取路径信息失败: {}", path.display()))?;
        if metadata.is_dir() {
            anyhow::ensure!(recursive, "删除目录必须设置 recursive=true");
            fs::remove_dir_all(&path).await?;
        } else {
            fs::remove_file(&path).await?;
        }
        Ok(ToolOutput::ok(format!("已删除 {}", path.display())))
    }
}

struct FileInfoTool;

#[async_trait]
impl Tool for FileInfoTool {
    fn name(&self) -> &'static str {
        "file_info"
    }
    fn description(&self) -> &'static str {
        "查看工作目录内文件或目录的类型、大小和修改时间"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        let metadata = fs::metadata(&path)
            .await
            .with_context(|| format!("读取路径信息失败: {}", path.display()))?;
        let kind = if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        Ok(ToolOutput::ok(format!(
            "path: {}\ntype: {}\nsize: {}\nreadonly: {}\nmodified_unix: {}",
            path.strip_prefix(&context.cwd).unwrap_or(&path).display(),
            kind,
            metadata.len(),
            metadata.permissions().readonly(),
            modified
        )))
    }
}

struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "在隔离的工作区副本中执行 shell 命令，执行前请求确认；sandbox=false 时在当前目录执行"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"command":{"type":"string"},"timeout":{"type":"integer","description":"超时时间，单位秒，默认 30"},"sandbox":{"type":"boolean","description":"是否在临时工作区副本中执行，默认 true"}},"required":["command"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let command = string_arg(&args, "command")?;
        let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
        let sandbox = bool_arg(&args, "sandbox", true);
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!(
                    "执行命令{}: {command}",
                    if sandbox {
                        "（沙箱）"
                    } else {
                        "（当前工作区）"
                    }
                ),
                diff: None,
            })
            .await?;
        run_shell_command(&command, &context.cwd, timeout_secs, sandbox).await
    }
}

struct GlobSearchTool;

#[async_trait]
impl Tool for GlobSearchTool {
    fn name(&self) -> &'static str {
        "glob_search"
    }
    fn description(&self) -> &'static str {
        "按 glob 模式查找工作目录内文件"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let pattern = string_arg(&args, "pattern")?;
        let matcher = Pattern::new(&pattern)?;
        let mut matches = Vec::new();
        for path in searchable_files(&context.cwd, &context.cwd) {
            let rel = path.strip_prefix(&context.cwd).unwrap_or(&path);
            if matcher.matches_path(rel) {
                matches.push(rel.display().to_string());
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
    fn name(&self) -> &'static str {
        "grep_search"
    }
    fn description(&self) -> &'static str {
        "在工作目录内递归搜索文本片段"
    }
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
        for path in searchable_files(&root, &context.cwd) {
            if !is_text_like(&path) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (line_number, line) in content.lines().enumerate() {
                if line.contains(&pattern) {
                    let rel = path.strip_prefix(&context.cwd).unwrap_or(&path).display();
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

struct CodeIndexTool;

#[async_trait]
impl Tool for CodeIndexTool {
    fn name(&self) -> &'static str {
        "code_index"
    }

    fn description(&self) -> &'static str {
        "构建轻量代码索引：尊重 .gitignore 与大文件过滤，支持按查询返回相关文件、符号和引用"
    }

    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "query":{"type":"string","description":"可选，按名称或文本过滤文件、符号和引用"},
                "path":{"type":"string","description":"可选，索引子目录，默认当前目录"},
                "limit":{"type":"integer","description":"返回数量上限，默认 100，最大 500"},
                "include_references":{"type":"boolean","description":"是否返回文本引用，默认 true"}
            }
        })
    }

    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let root = match args.get("path").and_then(Value::as_str) {
            Some(path) => resolve_path(&context.cwd, path)?,
            None => context.cwd.clone(),
        };
        let query = optional_string_arg(&args, "query").map(|value| value.to_lowercase());
        let limit = optional_u64_arg(&args, "limit", 100).clamp(1, 500) as usize;
        let include_references = bool_arg(&args, "include_references", true);
        let files = searchable_files(&root, &context.cwd);
        let mut records = Vec::new();
        let mut skipped_binary = 0usize;

        for path in files {
            if !is_text_like(&path) {
                skipped_binary += 1;
                continue;
            }
            let rel = path
                .strip_prefix(&context.cwd)
                .unwrap_or(&path)
                .display()
                .to_string();
            let Ok(content) = std::fs::read_to_string(&path) else {
                skipped_binary += 1;
                continue;
            };
            let symbols = extract_symbols(&path, &content);
            let file_matches = query
                .as_ref()
                .is_none_or(|query| rel.to_lowercase().contains(query));
            if query.is_none()
                || file_matches
                || symbols
                    .iter()
                    .any(|symbol| symbol.matches_query(query.as_deref()))
            {
                records.push(format_code_index_file(&rel, &symbols));
                if records.len() >= limit {
                    break;
                }
            }
            if include_references {
                if let Some(query) = query.as_deref() {
                    for (line_number, line) in content.lines().enumerate() {
                        if line.to_lowercase().contains(query) {
                            records.push(format!(
                                "ref\t{}:{}\t{}",
                                rel,
                                line_number + 1,
                                line.trim()
                            ));
                            if records.len() >= limit {
                                break;
                            }
                        }
                    }
                }
            }
            if records.len() >= limit {
                break;
            }
        }

        let mut output = format!(
            "indexed_records: {}\nskipped_non_text_or_unreadable: {}\n\n",
            records.len(),
            skipped_binary
        );
        output.push_str(&records.join("\n"));
        Ok(ToolOutput::ok(output.trim_end().to_string()))
    }
}

struct GitManagerTool;

#[async_trait]
impl Tool for GitManagerTool {
    fn name(&self) -> &'static str {
        "git_manager"
    }

    fn description(&self) -> &'static str {
        "Git 原生集成：查看状态和 diff、生成提交信息、创建分支、commit、push、打开 PR、返回 code review 所需 diff"
    }

    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["status","diff","review_diff","message","create_branch","commit","push","open_pr"],"description":"要执行的 Git 动作"},
                "paths":{"type":"array","items":{"type":"string"},"description":"可选，限制 diff/status/commit 的路径"},
                "branch":{"type":"string","description":"create_branch 的分支名，或 push/open_pr 的当前分支提示"},
                "message":{"type":"string","description":"commit message；commit 省略时会根据变更生成"},
                "staged":{"type":"boolean","description":"diff/review_diff 是否查看暂存区，默认 false"},
                "all":{"type":"boolean","description":"commit 时是否 git add -A 后提交，默认 true"},
                "base":{"type":"string","description":"open_pr 的目标分支，可选"},
                "title":{"type":"string","description":"open_pr 标题，可选"},
                "body":{"type":"string","description":"open_pr 正文，可选"},
                "timeout":{"type":"integer","description":"超时时间，单位秒，默认 60"}
            },
            "required":["action"]
        })
    }

    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let action = string_arg(&args, "action")?;
        let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
        let paths = array_of_strings_arg(&args, "paths")?;
        match action.as_str() {
            "status" => git_status(&context.cwd, &paths, timeout_secs).await,
            "diff" => {
                git_diff_tool(
                    &context.cwd,
                    &paths,
                    bool_arg(&args, "staged", false),
                    timeout_secs,
                )
                .await
            }
            "review_diff" => {
                git_review_diff(
                    &context.cwd,
                    &paths,
                    bool_arg(&args, "staged", false),
                    timeout_secs,
                )
                .await
            }
            "message" => git_message(&context.cwd, &paths, timeout_secs).await,
            "create_branch" => {
                let branch = string_arg(&args, "branch")?;
                confirm_git_action(context, self.name(), format!("创建并切换分支: {branch}"))
                    .await?;
                run_command(
                    vec![
                        "git".to_string(),
                        "switch".to_string(),
                        "-c".to_string(),
                        branch,
                    ],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            "commit" => {
                let add_all = bool_arg(&args, "all", true);
                let message = match optional_string_arg(&args, "message") {
                    Some(message) => message,
                    None => generate_commit_message(&context.cwd, &paths, timeout_secs).await?,
                };
                confirm_git_action(context, self.name(), format!("提交 Git 变更: {message}"))
                    .await?;
                let mut rendered = String::new();
                if add_all {
                    let add_output = git_add(&context.cwd, &paths, timeout_secs).await?;
                    rendered.push_str(&add_output.content);
                    rendered.push('\n');
                }
                let commit_output = run_command(
                    vec![
                        "git".to_string(),
                        "commit".to_string(),
                        "-m".to_string(),
                        message,
                    ],
                    &context.cwd,
                    timeout_secs,
                )
                .await?;
                rendered.push_str(&commit_output.content);
                Ok(ToolOutput {
                    content: rendered,
                    is_error: commit_output.is_error,
                })
            }
            "push" => {
                let branch = optional_string_arg(&args, "branch");
                confirm_git_action(context, self.name(), "推送当前 Git 分支".to_string()).await?;
                let command = if let Some(branch) = branch {
                    vec![
                        "git".to_string(),
                        "push".to_string(),
                        "-u".to_string(),
                        "origin".to_string(),
                        branch,
                    ]
                } else {
                    vec!["git".to_string(), "push".to_string()]
                };
                run_command(command, &context.cwd, timeout_secs).await
            }
            "open_pr" => {
                confirm_git_action(context, self.name(), "创建 GitHub Pull Request".to_string())
                    .await?;
                let mut command = vec![
                    "gh".to_string(),
                    "pr".to_string(),
                    "create".to_string(),
                    "--fill".to_string(),
                ];
                if let Some(base) = optional_string_arg(&args, "base") {
                    command.extend(["--base".to_string(), base]);
                }
                if let Some(title) = optional_string_arg(&args, "title") {
                    command.extend(["--title".to_string(), title]);
                }
                if let Some(body) = optional_string_arg(&args, "body") {
                    command.extend(["--body".to_string(), body]);
                }
                run_command(command, &context.cwd, timeout_secs).await
            }
            _ => anyhow::bail!("不支持的 Git 动作: {action}"),
        }
    }
}

struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &'static str {
        "list_dir"
    }
    fn description(&self) -> &'static str {
        "列出工作目录内某个目录的直接子项"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        let mut entries = tokio::fs::read_dir(&path)
            .await
            .with_context(|| format!("读取目录失败: {}", path.display()))?;
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

struct ListModelsTool;

#[async_trait]
impl Tool for ListModelsTool {
    fn name(&self) -> &'static str {
        "list_models"
    }
    fn description(&self) -> &'static str {
        "读取云智 API 当前可用模型列表，返回已清洗的模型 id，供主模型选择子智能体模型"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"hint":{"type":"string","description":"可选，说明本次模型选择用途"}}})
    }
    async fn execute(&self, _args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let client = ChatCompletionsClient::new(context.api_key.clone());
        let models = client.list_models().await?;
        let rendered = models
            .into_iter()
            .map(|model| match model.owned_by {
                Some(owner) => format!("{} ({})", model.id, owner),
                None => model.id,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput::ok(rendered))
    }
}

struct CallModelTool;

struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &'static str {
        "ask_user"
    }
    fn description(&self) -> &'static str {
        "向用户提出一个需要自由文本回答的问题，并把用户回答返回给模型"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "question":{"type":"string","description":"要询问用户的问题，必须简短明确"},
                "context":{"type":"string","description":"可选，说明为什么需要这个信息"},
                "default_answer":{"type":"string","description":"可选，用户直接回车时采用的默认答案"}
            },
            "required":["question"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let question = string_arg(&args, "question")?;
        let response = context
            .prompter
            .ask_user(UserQuestionRequest {
                question,
                context: args
                    .get("context")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                default_answer: args
                    .get("default_answer")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            })
            .await?;
        Ok(ToolOutput::ok(response))
    }
}

struct ChooseOptionTool;

#[async_trait]
impl Tool for ChooseOptionTool {
    fn name(&self) -> &'static str {
        "choose_option"
    }
    fn description(&self) -> &'static str {
        "让用户从一组候选项中选择一个选项，可选支持自定义输入，并把选择结果返回给模型"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "question":{"type":"string","description":"要让用户选择的问题，必须简短明确"},
                "context":{"type":"string","description":"可选，说明每个选项的背景或取舍"},
                "options":{"type":"array","items":{"type":"string"},"description":"候选项列表，至少 2 项"},
                "allow_custom":{"type":"boolean","description":"是否允许用户输入不在候选项内的自定义答案，默认 false"}
            },
            "required":["question","options"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let question = string_arg(&args, "question")?;
        let options = args
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("options 必须是字符串数组"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(ToString::to_string)
                    .ok_or_else(|| anyhow!("options 必须全部是字符串"))
            })
            .collect::<Result<Vec<_>>>()?;
        anyhow::ensure!(options.len() >= 2, "options 至少需要 2 项");
        let response = context
            .prompter
            .choose_option(UserChoiceRequest {
                question,
                context: args
                    .get("context")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                options,
                allow_custom: args
                    .get("allow_custom")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
            .await?;
        Ok(ToolOutput::ok(serde_json::to_string_pretty(&json!({
            "answer": response.answer,
            "index": response.index,
            "custom": response.custom,
        }))?))
    }
}

struct ListSkillsTool;

#[async_trait]
impl Tool for ListSkillsTool {
    fn name(&self) -> &'static str {
        "list_skills"
    }
    fn description(&self) -> &'static str {
        "列出项目级和用户级 Skills，来源为 .yunzhi/skills 与 ~/.yunzhi/skills"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"hint":{"type":"string","description":"可选，说明要寻找的技能类型"}}})
    }
    async fn execute(&self, _args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let skills = skills_index(&context.cwd)?;
        if skills.is_empty() {
            return Ok(ToolOutput::ok("未发现 Skills"));
        }
        let rendered = skills
            .into_iter()
            .map(|skill| {
                format!(
                    "{}\t{}\t{}",
                    skill.id,
                    skill.description,
                    skill.path.display()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput::ok(rendered))
    }
}

struct ReadSkillTool;

#[async_trait]
impl Tool for ReadSkillTool {
    fn name(&self) -> &'static str {
        "read_skill"
    }
    fn description(&self) -> &'static str {
        "读取指定 Skill 的完整 Markdown 说明，按 id 或路径查找"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"skill":{"type":"string","description":"Skill id 或路径"}},"required":["skill"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let (skill, content) = read_skill(&context.cwd, &string_arg(&args, "skill")?)?;
        Ok(ToolOutput::ok(format!(
            "Skill: {}\nPath: {}\n\n{}",
            skill.id,
            skill.path.display(),
            content
        )))
    }
}

struct ListMcpServersTool;

#[async_trait]
impl Tool for ListMcpServersTool {
    fn name(&self) -> &'static str {
        "list_mcp_servers"
    }
    fn description(&self) -> &'static str {
        "列出 .yunzhi/mcp.json 与 ~/.yunzhi/mcp.json 中配置的 MCP stdio server"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"hint":{"type":"string","description":"可选，说明要寻找的 MCP 能力"}}})
    }
    async fn execute(&self, _args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let servers = load_mcp_servers(&context.cwd)?;
        if servers.is_empty() {
            return Ok(ToolOutput::ok("未配置 MCP servers"));
        }
        let rendered = servers
            .into_iter()
            .map(|(name, server)| {
                let args = if server.args.is_empty() {
                    String::new()
                } else {
                    format!(
                        " {}",
                        shell_words::join(server.args.iter().map(String::as_str))
                    )
                };
                format!("{}\t{}{}", name, server.command, args)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput::ok(rendered))
    }
}

struct CallMcpTool;

#[async_trait]
impl Tool for CallMcpTool {
    fn name(&self) -> &'static str {
        "call_mcp_tool"
    }
    fn description(&self) -> &'static str {
        "通过 stdio JSON-RPC 调用已配置 MCP server 的工具，执行前请求确认"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "server":{"type":"string","description":"MCP server 名称"},
                "tool":{"type":"string","description":"MCP 工具名称"},
                "arguments":{"type":"object","description":"传给 MCP 工具的 JSON 参数"},
                "timeout":{"type":"integer","description":"超时时间，单位秒，默认 30"}
            },
            "required":["server","tool"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let server = string_arg(&args, "server")?;
        let tool = string_arg(&args, "tool")?;
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
        anyhow::ensure!(arguments.is_object(), "arguments 必须是对象");
        let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!("调用 MCP 工具 {server}/{tool}"),
                diff: None,
            })
            .await?;
        let result = call_mcp_tool(&context.cwd, &server, &tool, arguments, timeout_secs).await?;
        Ok(ToolOutput::ok(serde_json::to_string_pretty(&result)?))
    }
}

struct McpResourceTool;

#[async_trait]
impl Tool for McpResourceTool {
    fn name(&self) -> &'static str {
        "mcp_resource"
    }
    fn description(&self) -> &'static str {
        "按 MCP 官方 resources/list 与 resources/read 读取已配置 MCP server 的资源"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "server":{"type":"string","description":"MCP server 名称"},
                "action":{"type":"string","enum":["list","read"],"description":"list 列资源，read 读取资源"},
                "uri":{"type":"string","description":"action=read 时的资源 URI"},
                "timeout":{"type":"integer","description":"超时时间，单位秒，默认 30"}
            },
            "required":["server","action"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let server = string_arg(&args, "server")?;
        let action = string_arg(&args, "action")?;
        let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
        let result = match action.as_str() {
            "list" => list_mcp_resources(&context.cwd, &server, timeout_secs).await?,
            "read" => {
                let uri = string_arg(&args, "uri")?;
                read_mcp_resource(&context.cwd, &server, &uri, timeout_secs).await?
            }
            other => anyhow::bail!("未知 mcp_resource action: {other}"),
        };
        Ok(ToolOutput::ok(serde_json::to_string_pretty(&result)?))
    }
}

struct McpPromptTool;

#[async_trait]
impl Tool for McpPromptTool {
    fn name(&self) -> &'static str {
        "mcp_prompt"
    }
    fn description(&self) -> &'static str {
        "按 MCP 官方 prompts/list 与 prompts/get 列出和获取已配置 MCP server 的提示模板"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "server":{"type":"string","description":"MCP server 名称"},
                "action":{"type":"string","enum":["list","get"],"description":"list 列模板，get 获取模板"},
                "name":{"type":"string","description":"action=get 时的 prompt 名称"},
                "arguments":{"type":"object","description":"prompt 模板参数"},
                "timeout":{"type":"integer","description":"超时时间，单位秒，默认 30"}
            },
            "required":["server","action"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let server = string_arg(&args, "server")?;
        let action = string_arg(&args, "action")?;
        let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
        let result = match action.as_str() {
            "list" => list_mcp_prompts(&context.cwd, &server, timeout_secs).await?,
            "get" => {
                let name = string_arg(&args, "name")?;
                let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
                anyhow::ensure!(arguments.is_object(), "arguments 必须是对象");
                get_mcp_prompt(&context.cwd, &server, &name, arguments, timeout_secs).await?
            }
            other => anyhow::bail!("未知 mcp_prompt action: {other}"),
        };
        Ok(ToolOutput::ok(serde_json::to_string_pretty(&result)?))
    }
}

#[async_trait]
impl Tool for CallModelTool {
    fn name(&self) -> &'static str {
        "call_model"
    }
    fn description(&self) -> &'static str {
        "调用其他模型完成子任务，例如低成本总结、代码审阅、交叉检查或专门推理"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "model":{"type":"string","description":"要调用的模型名称，用于委托非当前主控模型完成子任务"},
                "prompt":{"type":"string","description":"发送给目标模型的任务内容"},
                "system":{"type":"string","description":"可选 system 指令"},
                "max_tokens":{"type":"integer","description":"最大输出 token，默认 2048"}
            },
            "required":["model","prompt"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let model = string_arg(&args, "model")?.trim().to_string();
        anyhow::ensure!(!model.is_empty(), "model 不能为空");
        let prompt = string_arg(&args, "prompt")?;
        let system = args.get("system").and_then(Value::as_str);
        let max_tokens = optional_u64_arg(&args, "max_tokens", 2048).clamp(1, 16_000) as u32;
        let client = ChatCompletionsClient::new(context.api_key.clone());
        let response = client
            .complete_once(&model, system, &prompt, max_tokens)
            .await
            .with_context(|| format!("调用模型失败: {model}"))?;
        Ok(ToolOutput::ok(response))
    }
}

struct ExecuteCodeTool;

#[async_trait]
impl Tool for ExecuteCodeTool {
    fn name(&self) -> &'static str {
        "execute_code"
    }
    fn description(&self) -> &'static str {
        "执行短代码片段并返回输出，支持 python、node、bash、rust-script 风格入口"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "language":{"type":"string","enum":["python","javascript","node","bash","sh","rust"],"description":"代码语言"},
                "code":{"type":"string","description":"要执行的代码片段"},
                "timeout":{"type":"integer","description":"超时时间，单位秒，默认 30"},
                "sandbox":{"type":"boolean","description":"是否在临时工作区副本中执行，默认 true"}
            },
            "required":["language","code"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let language = string_arg(&args, "language")?.to_lowercase();
        let code = string_arg(&args, "code")?;
        let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
        let sandbox = bool_arg(&args, "sandbox", true);
        let command = match language.as_str() {
            "python" => vec!["python3".to_string(), "-c".to_string(), code.clone()],
            "javascript" | "node" => vec!["node".to_string(), "-e".to_string(), code.clone()],
            "bash" | "sh" => vec!["sh".to_string(), "-c".to_string(), code.clone()],
            "rust" => vec!["rust-script".to_string(), "-e".to_string(), code.clone()],
            _ => anyhow::bail!("不支持的语言: {language}"),
        };
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!(
                    "执行 {} 代码片段{}",
                    language,
                    if sandbox {
                        "（沙箱）"
                    } else {
                        "（当前工作区）"
                    }
                ),
                diff: None,
            })
            .await?;
        run_command_with_sandbox(command, &context.cwd, timeout_secs, sandbox).await
    }
}

struct RunProgramTool;

#[async_trait]
impl Tool for RunProgramTool {
    fn name(&self) -> &'static str {
        "run_program"
    }
    fn description(&self) -> &'static str {
        "运行工作目录内或 PATH 中的程序，参数以数组传入，执行前请求确认"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "program":{"type":"string","description":"程序路径或 PATH 中的可执行文件名"},
                "args":{"type":"array","items":{"type":"string"},"description":"程序参数"},
                "timeout":{"type":"integer","description":"超时时间，单位秒，默认 30"}
            },
            "required":["program"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let program = string_arg(&args, "program")?;
        let mut command = vec![program.clone()];
        if let Some(values) = args.get("args").and_then(Value::as_array) {
            for value in values {
                command.push(
                    value
                        .as_str()
                        .ok_or_else(|| anyhow!("args 必须全部是字符串"))?
                        .to_string(),
                );
            }
        }
        let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!(
                    "运行程序: {}",
                    shell_words::join(command.iter().map(String::as_str))
                ),
                diff: None,
            })
            .await?;
        run_command(command, &context.cwd, timeout_secs).await
    }
}

struct TestLoopTool;

#[async_trait]
impl Tool for TestLoopTool {
    fn name(&self) -> &'static str {
        "test_loop"
    }

    fn description(&self) -> &'static str {
        "运行项目测试命令并返回可用于自动修复循环的失败摘要；未提供 command 时自动探测 cargo/npm/pytest 等命令"
    }

    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "command":{"type":"string","description":"可选测试命令；省略时自动探测"},
                "timeout":{"type":"integer","description":"超时时间，单位秒，默认 120"},
                "sandbox":{"type":"boolean","description":"是否在临时工作区副本中执行，默认 true"},
                "max_attempts":{"type":"integer","description":"本工具内部重复运行次数，默认 1；自动修复由 agent 根据失败输出继续编辑并再次调用"}
            }
        })
    }

    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let command = match optional_string_arg(&args, "command") {
            Some(command) if !command.trim().is_empty() => command,
            _ => detect_test_command(&context.cwd)?,
        };
        let timeout_secs = optional_u64_arg(&args, "timeout", 120).clamp(1, 1800);
        let sandbox = bool_arg(&args, "sandbox", true);
        let max_attempts = optional_u64_arg(&args, "max_attempts", 1).clamp(1, 10);
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!(
                    "运行测试{}: {command}",
                    if sandbox {
                        "（沙箱）"
                    } else {
                        "（当前工作区）"
                    }
                ),
                diff: None,
            })
            .await?;

        let mut attempts = Vec::new();
        let mut is_error = true;
        for attempt in 1..=max_attempts {
            let output = run_shell_command(&command, &context.cwd, timeout_secs, sandbox).await?;
            is_error = output.is_error;
            attempts.push(format!(
                "attempt {attempt}/{max_attempts}\n{}",
                format_test_output(&output.content)
            ));
            if !is_error {
                break;
            }
        }

        let status = if is_error { "failed" } else { "passed" };
        Ok(ToolOutput {
            content: format!(
                "test_status: {status}\ncommand: {command}\n\n{}",
                attempts.join("\n\n")
            ),
            is_error,
        })
    }
}

struct ManageTodosTool;

#[async_trait]
impl Tool for ManageTodosTool {
    fn name(&self) -> &'static str {
        "manage_todos"
    }
    fn description(&self) -> &'static str {
        "管理和跟踪当前会话内的代办任务，支持 add、update、list、clear"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["add","update","list","clear"]},
                "id":{"type":"integer","description":"update 时需要的任务 id"},
                "title":{"type":"string","description":"add/update 的任务标题"},
                "status":{"type":"string","enum":["pending","in_progress","done","blocked"],"description":"任务状态"},
                "notes":{"type":"string","description":"任务备注"}
            },
            "required":["action"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        match string_arg(&args, "action")?.as_str() {
            "add" => {
                let title = string_arg(&args, "title")?;
                let status = parse_todo_status(
                    args.get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("pending"),
                )?;
                let item = TodoItem {
                    id: context.next_todo_id,
                    title,
                    status,
                    notes: args
                        .get("notes")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                };
                context.next_todo_id += 1;
                context.todos.push(item);
                Ok(ToolOutput::ok(render_todos(&context.todos)))
            }
            "update" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("update 需要 id"))?;
                let item = context
                    .todos
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or_else(|| anyhow!("未找到代办任务: {id}"))?;
                if let Some(title) = args.get("title").and_then(Value::as_str) {
                    item.title = title.to_string();
                }
                if let Some(status) = args.get("status").and_then(Value::as_str) {
                    item.status = parse_todo_status(status)?;
                }
                if args.get("notes").is_some() {
                    item.notes = args
                        .get("notes")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                }
                Ok(ToolOutput::ok(render_todos(&context.todos)))
            }
            "list" => Ok(ToolOutput::ok(render_todos(&context.todos))),
            "clear" => {
                context.todos.clear();
                context.next_todo_id = 1;
                Ok(ToolOutput::ok("已清空代办任务"))
            }
            action => anyhow::bail!("不支持的代办操作: {action}"),
        }
    }
}

struct SystemControlTool;

#[async_trait]
impl Tool for SystemControlTool {
    fn name(&self) -> &'static str {
        "system_control"
    }
    fn description(&self) -> &'static str {
        "执行受控系统操作：查看环境、当前目录、进程列表、磁盘信息、终止进程"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "action":{"type":"string","enum":["pwd","env","processes","disk","kill_process"]},
                "pid":{"type":"integer","description":"kill_process 的进程 id"},
                "signal":{"type":"string","description":"kill_process 的信号，默认 TERM"}
            },
            "required":["action"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        match string_arg(&args, "action")?.as_str() {
            "pwd" => Ok(ToolOutput::ok(context.cwd.display().to_string())),
            "env" => {
                let mut vars = std::env::vars()
                    .filter(|(key, _)| {
                        !key.to_lowercase().contains("key")
                            && !key.to_lowercase().contains("token")
                            && !key.to_lowercase().contains("secret")
                    })
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>();
                vars.sort();
                Ok(ToolOutput::ok(vars.join("\n")))
            }
            "processes" => {
                run_command(
                    vec![
                        "ps".to_string(),
                        "-eo".to_string(),
                        "pid,ppid,comm,args".to_string(),
                    ],
                    &context.cwd,
                    10,
                )
                .await
            }
            "disk" => {
                run_command(
                    vec!["df".to_string(), "-h".to_string(), ".".to_string()],
                    &context.cwd,
                    10,
                )
                .await
            }
            "kill_process" => {
                let pid = args
                    .get("pid")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("kill_process 需要 pid"))?;
                let signal = args.get("signal").and_then(Value::as_str).unwrap_or("TERM");
                context
                    .confirm(PermissionRequest {
                        tool_name: self.name().to_string(),
                        summary: format!("发送 SIG{} 到进程 {}", signal, pid),
                        diff: None,
                    })
                    .await?;
                run_command(
                    vec!["kill".to_string(), format!("-{}", signal), pid.to_string()],
                    &context.cwd,
                    10,
                )
                .await
            }
            action => anyhow::bail!("不支持的系统操作: {action}"),
        }
    }
}

struct CreatePresentationTool;

#[async_trait]
impl Tool for CreatePresentationTool {
    fn name(&self) -> &'static str {
        "create_presentation"
    }
    fn description(&self) -> &'static str {
        "生成 PPT 大纲、Marp Markdown 或 PPTX/POTX 演示文稿文件"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "path":{"type":"string","description":"输出文件路径，支持 .md/.pptx/.potx"},
                "title":{"type":"string","description":"演示文稿标题"},
                "audience":{"type":"string","description":"目标受众"},
                "slides":{"type":"array","items":{"type":"object"},"description":"幻灯片数组，每项可含 title、bullets、speaker_notes、image_prompt"},
                "theme":{"type":"string","description":"可选，视觉主题"},
                "format":{"type":"string","enum":["markdown","ppt","pptx","potx"]}
            },
            "required":["path","title","slides"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        let title = string_arg(&args, "title")?;
        let audience =
            optional_string_arg(&args, "audience").unwrap_or_else(|| "通用受众".to_string());
        let theme = optional_string_arg(&args, "theme").unwrap_or_else(|| "default".to_string());
        let slides = args
            .get("slides")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("slides 必须是数组"))?;
        anyhow::ensure!(!slides.is_empty(), "slides 不能为空");
        let format = normalize_format(&args, &path, "markdown");

        if matches!(format.as_str(), "ppt" | "pptx" | "potx") {
            let presentation = collect_presentation(&title, &audience, slides);
            let content = build_pptx(&presentation, format == "potx")?;
            return write_binary_file_with_confirmation(
                self.name(),
                &path,
                content,
                context,
                format!("生成演示文稿 {}", path.display()),
            )
            .await;
        }

        let mut content = format!(
            "---\nmarp: true\ntheme: {}\npaginate: true\n---\n\n# {}\n\n目标受众：{}\n",
            theme, title, audience
        );
        for slide in slides {
            content.push_str("\n---\n\n");
            let slide_title = slide
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("未命名页面");
            content.push_str(&format!("## {}\n", slide_title));
            if let Some(items) = slide.get("bullets").and_then(Value::as_array) {
                for item in items.iter().filter_map(Value::as_str) {
                    content.push_str(&format!("- {}\n", item));
                }
            }
            if let Some(prompt) = slide.get("image_prompt").and_then(Value::as_str) {
                content.push_str(&format!("\n> 配图提示词：{}\n", prompt));
            }
            if let Some(notes) = slide.get("speaker_notes").and_then(Value::as_str) {
                content.push_str(&format!("\n<!-- 演讲备注：{} -->\n", notes));
            }
        }
        write_text_file_with_confirmation(
            self.name(),
            &path,
            content,
            context,
            format!("生成演示文稿 {}", path.display()),
        )
        .await
    }
}

struct GenerateImageTool;

#[async_trait]
impl Tool for GenerateImageTool {
    fn name(&self) -> &'static str {
        "generate_image"
    }
    fn description(&self) -> &'static str {
        "调用绘图模型生成图片提示、SVG 草图或保存模型返回的图片结果"
    }
    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "prompt":{"type":"string","description":"绘图提示词"},
                "model":{"type":"string","description":"绘图或多模态模型名称，默认 Image-Generation"},
                "path":{"type":"string","description":"可选，保存输出文本/SVG/URL 的文件路径"},
                "style":{"type":"string","description":"可选，风格约束"},
                "size":{"type":"string","description":"可选，如 1024x1024"},
                "save_response":{"type":"boolean","description":"是否保存模型返回内容，默认 true"}
            },
            "required":["prompt"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let prompt = string_arg(&args, "prompt")?;
        let model =
            optional_string_arg(&args, "model").unwrap_or_else(|| "Image-Generation".to_string());
        let style = optional_string_arg(&args, "style")
            .unwrap_or_else(|| "clean, high quality".to_string());
        let size = optional_string_arg(&args, "size").unwrap_or_else(|| "1024x1024".to_string());
        let request = format!(
            "请作为绘图模型生成图片。提示词：{}\n风格：{}\n尺寸：{}\n如果无法直接返回二进制图片，请返回可下载 URL、base64、SVG 或详细的最终绘图提示。",
            prompt, style, size
        );
        let client = ChatCompletionsClient::new(context.api_key.clone());
        let response = client
            .complete_once(
                &model,
                Some("你是图像生成与视觉提示词模型。"),
                &request,
                4096,
            )
            .await
            .with_context(|| format!("调用绘图模型失败: {model}"))?;
        if bool_arg(&args, "save_response", true) {
            if let Some(raw_path) = args.get("path").and_then(Value::as_str) {
                let path = resolve_path(&context.cwd, raw_path)?;
                return write_text_file_with_confirmation(
                    self.name(),
                    &path,
                    response,
                    context,
                    format!("保存图片生成结果 {}", path.display()),
                )
                .await;
            }
        }
        Ok(ToolOutput::ok(response))
    }
}

struct WriteDocumentTool;

#[async_trait]
impl Tool for WriteDocumentTool {
    fn name(&self) -> &'static str {
        "write_document"
    }
    fn description(&self) -> &'static str {
        "按主题、结构和内容写 Markdown/文本/HTML/Word/PDF/ODT/RTF/EPUB 文档"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"title":{"type":"string"},"sections":{"type":"array","items":{"type":"object"}},"format":{"type":"string","enum":["markdown","text","html","word","docx","dotx","odt","rtf","pdf","epub"]}},"required":["path","title","sections"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        let title = string_arg(&args, "title")?;
        let format = optional_string_arg(&args, "format").unwrap_or_else(|| "markdown".to_string());
        let sections = args
            .get("sections")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("sections 必须是数组"))?;
        let document = collect_document(&title, sections);
        if matches!(
            format.as_str(),
            "word" | "docx" | "dotx" | "odt" | "rtf" | "pdf" | "epub"
        ) {
            return write_office_document(
                self.name(),
                &path,
                &format,
                OfficeContent::Document(document),
                context,
            )
            .await;
        }
        let mut content = match format.as_str() {
            "html" => format!("<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{}</title></head><body>\n<h1>{}</h1>\n", title, title),
            "text" => format!("{}\n{}\n\n", title, "=".repeat(title.chars().count())),
            _ => format!("# {}\n\n", title),
        };
        for section in sections {
            let heading = section
                .get("heading")
                .and_then(Value::as_str)
                .unwrap_or("小节");
            let body = section
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match format.as_str() {
                "html" => content.push_str(&format!("<h2>{}</h2>\n<p>{}</p>\n", heading, body)),
                "text" => content.push_str(&format!(
                    "{}\n{}\n\n{}\n\n",
                    heading,
                    "-".repeat(heading.chars().count()),
                    body
                )),
                _ => content.push_str(&format!("## {}\n\n{}\n\n", heading, body)),
            }
        }
        if format == "html" {
            content.push_str("</body></html>\n");
        }
        write_text_file_with_confirmation(
            self.name(),
            &path,
            content,
            context,
            format!("写文档 {}", path.display()),
        )
        .await
    }
}

struct WriteTableTool;

#[async_trait]
impl Tool for WriteTableTool {
    fn name(&self) -> &'static str {
        "write_table"
    }
    fn description(&self) -> &'static str {
        "生成 Markdown、CSV、TSV、Excel XLSX/XLTX/XLS 或 LibreOffice Calc ODS 表格文件"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"columns":{"type":"array","items":{"type":"string"}},"rows":{"type":"array","items":{"type":"array"}},"format":{"type":"string","enum":["markdown","csv","tsv","excel","xlsx","xls","xltx","ods","calc"]}},"required":["path","columns","rows"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        let columns = array_of_strings_arg(&args, "columns")?;
        anyhow::ensure!(!columns.is_empty(), "columns 不能为空");
        let rows = args
            .get("rows")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("rows 必须是数组"))?;
        let format = optional_string_arg(&args, "format").unwrap_or_else(|| "markdown".to_string());
        let table_rows = collect_table_rows(&columns, rows)?;
        if matches!(format.as_str(), "ods" | "calc" | "excel" | "xlsx" | "xltx") {
            return write_office_document(
                self.name(),
                &path,
                &format,
                OfficeContent::Table(table_rows),
                context,
            )
            .await;
        }
        if matches!(format.as_str(), "tsv" | "xls") {
            let content = table_rows
                .iter()
                .map(|row| tsv_line(row))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            return write_text_file_with_confirmation(
                self.name(),
                &path,
                content,
                context,
                format!("写表格 {}", path.display()),
            )
            .await;
        }
        let mut content = String::new();
        if format == "csv" {
            content.push_str(&csv_line(&columns));
            content.push('\n');
            for row in rows {
                let values = row
                    .as_array()
                    .ok_or_else(|| anyhow!("rows 必须是二维数组"))?
                    .iter()
                    .map(value_to_cell)
                    .collect::<Vec<_>>();
                content.push_str(&csv_line(&values));
                content.push('\n');
            }
        } else {
            content.push_str(&format!("| {} |\n", columns.join(" | ")));
            content.push_str(&format!(
                "| {} |\n",
                columns
                    .iter()
                    .map(|_| "---")
                    .collect::<Vec<_>>()
                    .join(" | ")
            ));
            for row in rows {
                let values = row
                    .as_array()
                    .ok_or_else(|| anyhow!("rows 必须是二维数组"))?
                    .iter()
                    .map(value_to_cell)
                    .collect::<Vec<_>>();
                content.push_str(&format!("| {} |\n", values.join(" | ")));
            }
        }
        write_text_file_with_confirmation(
            self.name(),
            &path,
            content,
            context,
            format!("写表格 {}", path.display()),
        )
        .await
    }
}

struct OfficeDocumentTool;

#[async_trait]
impl Tool for OfficeDocumentTool {
    fn name(&self) -> &'static str {
        "office_document"
    }

    fn description(&self) -> &'static str {
        "生成 Word/PPT/Excel/PDF/ODT/RTF/DOCX/XLSX/PPTX/EPUB 等办公文档"
    }

    fn schema(&self) -> Value {
        json!({
            "type":"object",
            "properties":{
                "path":{"type":"string"},
                "format":{"type":"string","enum":["word","ppt","excel","pdf","odt","rtf","xlsx","ods","tsv","csv","xls","xltx","dotx","docx","potx","pptx","epub"]},
                "title":{"type":"string"},
                "sections":{"type":"array","items":{"type":"object"},"description":"文档/电子书章节，含 heading/body"},
                "slides":{"type":"array","items":{"type":"object"},"description":"演示文稿页面，含 title/bullets/speaker_notes"},
                "columns":{"type":"array","items":{"type":"string"}},
                "rows":{"type":"array","items":{"type":"array"}}
            },
            "required":["path","format"]
        })
    }

    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = resolve_path(&context.cwd, &string_arg(&args, "path")?)?;
        let format = normalize_format(&args, &path, "docx");
        let title = optional_string_arg(&args, "title").unwrap_or_else(|| "未命名文档".to_string());

        match office_kind(&format)? {
            OfficeKind::Document => {
                let sections = args
                    .get("sections")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_else(|| vec![json!({"heading":"正文","body":""})]);
                write_office_document(
                    self.name(),
                    &path,
                    &format,
                    OfficeContent::Document(collect_document(&title, &sections)),
                    context,
                )
                .await
            }
            OfficeKind::Presentation => {
                let slides = args
                    .get("slides")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("生成演示文稿需要 slides"))?;
                let presentation = collect_presentation(&title, "通用受众", slides);
                write_office_document(
                    self.name(),
                    &path,
                    &format,
                    OfficeContent::Presentation(presentation),
                    context,
                )
                .await
            }
            OfficeKind::Table => {
                let columns = array_of_strings_arg(&args, "columns")?;
                let rows = args
                    .get("rows")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("生成表格需要 rows"))?;
                write_office_document(
                    self.name(),
                    &path,
                    &format,
                    OfficeContent::Table(collect_table_rows(&columns, rows)?),
                    context,
                )
                .await
            }
        }
    }
}

struct DiskManagerTool;

#[async_trait]
impl Tool for DiskManagerTool {
    fn name(&self) -> &'static str {
        "disk_manager"
    }
    fn description(&self) -> &'static str {
        "管理磁盘和工作目录文件：usage、du、find_large、cleanup_empty_dirs"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["usage","du","find_large","cleanup_empty_dirs"]},"path":{"type":"string","description":"默认当前目录"},"limit":{"type":"integer","description":"find_large 返回数量，默认 20"}},"required":["action"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(path) => resolve_path(&context.cwd, path)?,
            None => context.cwd.clone(),
        };
        match string_arg(&args, "action")?.as_str() {
            "usage" => {
                run_command(
                    vec![
                        "df".to_string(),
                        "-h".to_string(),
                        path.display().to_string(),
                    ],
                    &context.cwd,
                    20,
                )
                .await
            }
            "du" => {
                run_command(
                    vec![
                        "du".to_string(),
                        "-sh".to_string(),
                        path.display().to_string(),
                    ],
                    &context.cwd,
                    60,
                )
                .await
            }
            "find_large" => {
                let limit = optional_u64_arg(&args, "limit", 20)
                    .clamp(1, 200)
                    .to_string();
                run_command(
                    vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        format!(
                            "find {} -type f -printf '%s %p\\n' | sort -nr | head -n {}",
                            shell_words::quote(&path.display().to_string()),
                            limit
                        ),
                    ],
                    &context.cwd,
                    120,
                )
                .await
            }
            "cleanup_empty_dirs" => {
                context
                    .confirm(PermissionRequest {
                        tool_name: self.name().to_string(),
                        summary: format!("清理空目录 {}", path.display()),
                        diff: None,
                    })
                    .await?;
                run_command(
                    vec![
                        "find".to_string(),
                        path.display().to_string(),
                        "-type".to_string(),
                        "d".to_string(),
                        "-empty".to_string(),
                        "-delete".to_string(),
                    ],
                    &context.cwd,
                    120,
                )
                .await
            }
            action => anyhow::bail!("不支持的磁盘操作: {action}"),
        }
    }
}

struct ComputerManagerTool;

#[async_trait]
impl Tool for ComputerManagerTool {
    fn name(&self) -> &'static str {
        "computer_manager"
    }
    fn description(&self) -> &'static str {
        "管理电脑任务：进程、服务状态、打开路径/URL、执行安全受控命令"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["processes","open","service_status","run"]},"target":{"type":"string"},"command":{"type":"string"},"timeout":{"type":"integer"}},"required":["action"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        match string_arg(&args, "action")?.as_str() {
            "processes" => {
                run_command(
                    vec![
                        "ps".to_string(),
                        "-eo".to_string(),
                        "pid,ppid,stat,comm,args".to_string(),
                    ],
                    &context.cwd,
                    20,
                )
                .await
            }
            "open" => {
                let target = string_arg(&args, "target")?;
                context
                    .confirm(PermissionRequest {
                        tool_name: self.name().to_string(),
                        summary: format!("打开 {}", target),
                        diff: None,
                    })
                    .await?;
                run_command(vec!["xdg-open".to_string(), target], &context.cwd, 20).await
            }
            "service_status" => {
                run_command(
                    vec![
                        "systemctl".to_string(),
                        "status".to_string(),
                        string_arg(&args, "target")?,
                        "--no-pager".to_string(),
                    ],
                    &context.cwd,
                    20,
                )
                .await
            }
            "run" => {
                let command = string_arg(&args, "command")?;
                let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
                context
                    .confirm(PermissionRequest {
                        tool_name: self.name().to_string(),
                        summary: format!("执行电脑管理命令: {command}"),
                        diff: None,
                    })
                    .await?;
                let output = timeout(
                    Duration::from_secs(timeout_secs),
                    Command::new("sh")
                        .arg("-c")
                        .arg(&command)
                        .current_dir(&context.cwd)
                        .output(),
                )
                .await
                .map_err(|_| anyhow!("命令超时，已终止: {command}"))??;
                Ok(render_process_output(command, output, timeout_secs))
            }
            action => anyhow::bail!("不支持的电脑管理操作: {action}"),
        }
    }
}

struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }
    fn description(&self) -> &'static str {
        "通过搜索 URL 拉取网页搜索结果摘要"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"query":{"type":"string"},"engine_url":{"type":"string","description":"默认 DuckDuckGo html 搜索"},"timeout":{"type":"integer"}},"required":["query"]})
    }
    async fn execute(&self, args: Value, _context: &mut ToolContext) -> Result<ToolOutput> {
        let query = string_arg(&args, "query")?;
        let engine = optional_string_arg(&args, "engine_url")
            .unwrap_or_else(|| "https://duckduckgo.com/html/?q=".to_string());
        let timeout_secs = optional_u64_arg(&args, "timeout", 20).clamp(1, 120);
        let url = format!("{}{}", engine, percent_encode_query(&query));
        let client = http_client(timeout_secs)?;
        let text = client
            .get(&url)
            .send()
            .await
            .context("网络搜索请求失败")?
            .text()
            .await
            .context("读取搜索响应失败")?;
        Ok(ToolOutput::ok(truncate_text(&strip_html(&text), 8000)))
    }
}

struct BrowserTool;

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &'static str {
        "browser"
    }
    fn description(&self) -> &'static str {
        "调用浏览器相关能力：fetch 网页文本或 open URL"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["fetch","open"]},"url":{"type":"string"},"timeout":{"type":"integer"}},"required":["action","url"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let url = string_arg(&args, "url")?;
        match string_arg(&args, "action")?.as_str() {
            "fetch" => {
                let text = http_client(optional_u64_arg(&args, "timeout", 20).clamp(1, 120))?
                    .get(&url)
                    .send()
                    .await
                    .context("浏览器 fetch 请求失败")?
                    .text()
                    .await
                    .context("读取网页失败")?;
                Ok(ToolOutput::ok(truncate_text(&strip_html(&text), 12_000)))
            }
            "open" => {
                context
                    .confirm(PermissionRequest {
                        tool_name: self.name().to_string(),
                        summary: format!("用系统浏览器打开 {}", url),
                        diff: None,
                    })
                    .await?;
                run_command(
                    vec!["xdg-open".to_string(), url],
                    &context.cwd,
                    optional_u64_arg(&args, "timeout", 20).clamp(1, 120),
                )
                .await
            }
            action => anyhow::bail!("不支持的浏览器操作: {action}"),
        }
    }
}

struct NetworkLogsTool;

#[async_trait]
impl Tool for NetworkLogsTool {
    fn name(&self) -> &'static str {
        "network_logs"
    }
    fn description(&self) -> &'static str {
        "获取网络日志和网络状态：connections、routes、dns、ping、curl_headers"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["connections","routes","dns","ping","curl_headers"]},"target":{"type":"string"},"timeout":{"type":"integer"}},"required":["action"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let timeout_secs = optional_u64_arg(&args, "timeout", 20).clamp(1, 120);
        match string_arg(&args, "action")?.as_str() {
            "connections" => {
                run_command(
                    vec!["ss".to_string(), "-tunap".to_string()],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            "routes" => {
                run_command(
                    vec!["ip".to_string(), "route".to_string()],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            "dns" => {
                run_command(
                    vec!["resolvectl".to_string(), "status".to_string()],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            "ping" => {
                run_command(
                    vec![
                        "ping".to_string(),
                        "-c".to_string(),
                        "4".to_string(),
                        string_arg(&args, "target")?,
                    ],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            "curl_headers" => {
                run_command(
                    vec![
                        "curl".to_string(),
                        "-I".to_string(),
                        string_arg(&args, "target")?,
                    ],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            action => anyhow::bail!("不支持的网络日志操作: {action}"),
        }
    }
}

struct ComputerInfoTool;

#[async_trait]
impl Tool for ComputerInfoTool {
    fn name(&self) -> &'static str {
        "computer_info"
    }
    fn description(&self) -> &'static str {
        "获取电脑信息：系统、CPU、内存、磁盘、网络、用户环境"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"scope":{"type":"string","enum":["summary","cpu","memory","disk","network","env"]}}})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        match args.get("scope").and_then(Value::as_str).unwrap_or("summary") {
            "cpu" => run_command(vec!["lscpu".to_string()], &context.cwd, 20).await,
            "memory" => run_command(vec!["free".to_string(), "-h".to_string()], &context.cwd, 20).await,
            "disk" => run_command(vec!["df".to_string(), "-h".to_string()], &context.cwd, 20).await,
            "network" => run_command(vec!["ip".to_string(), "addr".to_string()], &context.cwd, 20).await,
            "env" => SystemControlTool.execute(json!({"action":"env"}), context).await,
            "summary" => run_command(vec!["sh".to_string(), "-c".to_string(), "uname -a; echo; lscpu | head -20; echo; free -h; echo; df -h .; echo; ip route".to_string()], &context.cwd, 30).await,
            scope => anyhow::bail!("不支持的信息范围: {scope}"),
        }
    }
}

struct DatabaseManagerTool;

#[async_trait]
impl Tool for DatabaseManagerTool {
    fn name(&self) -> &'static str {
        "database_manager"
    }
    fn description(&self) -> &'static str {
        "连接和管理数据库，支持 sqlite 查询以及通过 psql/mysql/redis-cli 调用外部客户端"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"kind":{"type":"string","enum":["sqlite","postgres","mysql","redis"]},"action":{"type":"string","enum":["query","execute","schema","tables"]},"database":{"type":"string","description":"sqlite 文件路径或连接串/数据库名"},"query":{"type":"string"},"timeout":{"type":"integer"}},"required":["kind","action"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let kind = string_arg(&args, "kind")?;
        let action = string_arg(&args, "action")?;
        let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
        if matches!(action.as_str(), "execute") {
            context
                .confirm(PermissionRequest {
                    tool_name: self.name().to_string(),
                    summary: "执行数据库写入/变更语句".to_string(),
                    diff: None,
                })
                .await?;
        }
        match (kind.as_str(), action.as_str()) {
            ("sqlite", "tables") => {
                run_command(
                    vec![
                        "sqlite3".to_string(),
                        resolve_path(&context.cwd, &string_arg(&args, "database")?)?
                            .display()
                            .to_string(),
                        ".tables".to_string(),
                    ],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            ("sqlite", "schema") => {
                run_command(
                    vec![
                        "sqlite3".to_string(),
                        resolve_path(&context.cwd, &string_arg(&args, "database")?)?
                            .display()
                            .to_string(),
                        ".schema".to_string(),
                    ],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            ("sqlite", "query") | ("sqlite", "execute") => {
                run_command(
                    vec![
                        "sqlite3".to_string(),
                        resolve_path(&context.cwd, &string_arg(&args, "database")?)?
                            .display()
                            .to_string(),
                        string_arg(&args, "query")?,
                    ],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            ("postgres", "query") | ("postgres", "execute") => {
                run_command(
                    vec![
                        "psql".to_string(),
                        string_arg(&args, "database")?,
                        "-c".to_string(),
                        string_arg(&args, "query")?,
                    ],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            ("mysql", "query") | ("mysql", "execute") => {
                run_command(
                    vec![
                        "mysql".to_string(),
                        string_arg(&args, "database")?,
                        "-e".to_string(),
                        string_arg(&args, "query")?,
                    ],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            ("redis", "query") | ("redis", "execute") => {
                run_command(
                    vec!["redis-cli".to_string(), string_arg(&args, "query")?],
                    &context.cwd,
                    timeout_secs,
                )
                .await
            }
            _ => anyhow::bail!("不支持的数据库操作: {kind}/{action}"),
        }
    }
}

struct UiDesignTool;

#[async_trait]
impl Tool for UiDesignTool {
    fn name(&self) -> &'static str {
        "ui_design"
    }
    fn description(&self) -> &'static str {
        "生成 UI 智能设计规格、组件结构、文案和可保存的设计文档"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"brief":{"type":"string"},"path":{"type":"string"},"platform":{"type":"string"},"audience":{"type":"string"},"style":{"type":"string"}},"required":["brief"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let brief = string_arg(&args, "brief")?;
        let prompt = format!("请输出一份可执行 UI 设计规格，包含信息架构、关键界面、组件、状态、交互、响应式布局、视觉系统、可访问性与实现注意事项。\n平台：{}\n受众：{}\n风格：{}\n需求：{}", optional_string_arg(&args, "platform").unwrap_or_else(|| "web/desktop".to_string()), optional_string_arg(&args, "audience").unwrap_or_else(|| "目标用户".to_string()), optional_string_arg(&args, "style").unwrap_or_else(|| "克制、清晰、专业".to_string()), brief);
        let client = ChatCompletionsClient::new(context.api_key.clone());
        let response = client
            .complete_once(
                crate::llm::DEFAULT_MODEL,
                Some("你是资深产品设计师和前端架构师。"),
                &prompt,
                4096,
            )
            .await?;
        if let Some(raw_path) = args.get("path").and_then(Value::as_str) {
            let path = resolve_path(&context.cwd, raw_path)?;
            return write_text_file_with_confirmation(
                self.name(),
                &path,
                response,
                context,
                format!("保存 UI 设计文档 {}", path.display()),
            )
            .await;
        }
        Ok(ToolOutput::ok(response))
    }
}

struct LongMemoryTool;

#[async_trait]
impl Tool for LongMemoryTool {
    fn name(&self) -> &'static str {
        "long_memory"
    }
    fn description(&self) -> &'static str {
        "管理项目长期记忆 .yunzhi/memory.md，支持 read、append、replace、clear"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["read","append","replace","clear"]},"content":{"type":"string"}},"required":["action"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let path = context.cwd.join(".yunzhi").join("memory.md");
        match string_arg(&args, "action")?.as_str() {
            "read" => Ok(ToolOutput::ok(
                fs::read_to_string(&path).await.unwrap_or_default(),
            )),
            "append" => {
                let old = fs::read_to_string(&path).await.unwrap_or_default();
                let mut new = old.clone();
                if !new.ends_with('\n') && !new.is_empty() {
                    new.push('\n');
                }
                new.push_str(&string_arg(&args, "content")?);
                new.push('\n');
                write_text_file_with_confirmation(
                    self.name(),
                    &path,
                    new,
                    context,
                    "追加长期记忆".to_string(),
                )
                .await
            }
            "replace" => {
                write_text_file_with_confirmation(
                    self.name(),
                    &path,
                    string_arg(&args, "content")?,
                    context,
                    "替换长期记忆".to_string(),
                )
                .await
            }
            "clear" => {
                write_text_file_with_confirmation(
                    self.name(),
                    &path,
                    String::new(),
                    context,
                    "清空长期记忆".to_string(),
                )
                .await
            }
            action => anyhow::bail!("不支持的长期记忆操作: {action}"),
        }
    }
}

fn value_to_cell(value: &Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| value.to_string())
        .replace('\n', " ")
}

fn collect_table_rows(columns: &[String], rows: &[Value]) -> Result<Vec<Vec<String>>> {
    let mut table_rows = Vec::with_capacity(rows.len() + 1);
    table_rows.push(columns.to_vec());
    for row in rows {
        table_rows.push(
            row.as_array()
                .ok_or_else(|| anyhow!("rows 必须是二维数组"))?
                .iter()
                .map(value_to_cell)
                .collect(),
        );
    }
    Ok(table_rows)
}

#[derive(Clone)]
struct DocumentData {
    title: String,
    sections: Vec<DocumentSection>,
}

#[derive(Clone)]
struct DocumentSection {
    heading: String,
    body: String,
}

#[derive(Clone)]
struct PresentationData {
    title: String,
    audience: String,
    slides: Vec<SlideData>,
}

#[derive(Clone)]
struct SlideData {
    title: String,
    bullets: Vec<String>,
    notes: String,
}

enum OfficeContent {
    Document(DocumentData),
    Presentation(PresentationData),
    Table(Vec<Vec<String>>),
}

enum OfficeKind {
    Document,
    Presentation,
    Table,
}

fn normalize_format(args: &Value, path: &Path, default: &str) -> String {
    optional_string_arg(args, "format")
        .or_else(|| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
        })
        .unwrap_or_else(|| default.to_string())
        .to_ascii_lowercase()
}

fn office_kind(format: &str) -> Result<OfficeKind> {
    match format {
        "word" | "docx" | "dotx" | "odt" | "rtf" | "pdf" | "epub" => Ok(OfficeKind::Document),
        "ppt" | "pptx" | "potx" => Ok(OfficeKind::Presentation),
        "excel" | "xlsx" | "xltx" | "ods" | "calc" | "tsv" | "csv" | "xls" => Ok(OfficeKind::Table),
        other => anyhow::bail!("不支持的办公格式: {other}"),
    }
}

fn collect_document(title: &str, sections: &[Value]) -> DocumentData {
    DocumentData {
        title: title.to_string(),
        sections: sections
            .iter()
            .map(|section| DocumentSection {
                heading: section
                    .get("heading")
                    .and_then(Value::as_str)
                    .unwrap_or("小节")
                    .to_string(),
                body: section
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect(),
    }
}

fn collect_presentation(title: &str, audience: &str, slides: &[Value]) -> PresentationData {
    PresentationData {
        title: title.to_string(),
        audience: audience.to_string(),
        slides: slides
            .iter()
            .map(|slide| SlideData {
                title: slide
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("未命名页面")
                    .to_string(),
                bullets: slide
                    .get("bullets")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToString::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                notes: slide
                    .get("speaker_notes")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect(),
    }
}

async fn write_office_document(
    tool_name: &str,
    path: &Path,
    format: &str,
    content: OfficeContent,
    context: &mut ToolContext,
) -> Result<ToolOutput> {
    match (format, content) {
        ("word" | "docx" | "dotx", OfficeContent::Document(document)) => {
            write_binary_file_with_confirmation(
                tool_name,
                path,
                build_docx(&document, format == "dotx")?,
                context,
                format!("写 Word 文档 {}", path.display()),
            )
            .await
        }
        ("odt", OfficeContent::Document(document)) => {
            write_binary_file_with_confirmation(
                tool_name,
                path,
                build_odt(&document)?,
                context,
                format!("写 ODT 文档 {}", path.display()),
            )
            .await
        }
        ("rtf", OfficeContent::Document(document)) => {
            write_text_file_with_confirmation(
                tool_name,
                path,
                build_rtf(&document),
                context,
                format!("写 RTF 文档 {}", path.display()),
            )
            .await
        }
        ("pdf", OfficeContent::Document(document)) => {
            write_binary_file_with_confirmation(
                tool_name,
                path,
                build_pdf(&document),
                context,
                format!("写 PDF 文档 {}", path.display()),
            )
            .await
        }
        ("epub", OfficeContent::Document(document)) => {
            write_binary_file_with_confirmation(
                tool_name,
                path,
                build_epub(&document)?,
                context,
                format!("写 EPUB 文档 {}", path.display()),
            )
            .await
        }
        ("ppt" | "pptx" | "potx", OfficeContent::Presentation(presentation)) => {
            write_binary_file_with_confirmation(
                tool_name,
                path,
                build_pptx(&presentation, format == "potx")?,
                context,
                format!("写 PPT 演示文稿 {}", path.display()),
            )
            .await
        }
        ("excel" | "xlsx" | "xltx", OfficeContent::Table(rows)) => {
            write_binary_file_with_confirmation(
                tool_name,
                path,
                build_xlsx(&rows, format == "xltx")?,
                context,
                format!("写 Excel 表格 {}", path.display()),
            )
            .await
        }
        ("ods" | "calc", OfficeContent::Table(rows)) => {
            write_binary_file_with_confirmation(
                tool_name,
                path,
                build_ods(&rows)?,
                context,
                format!("写 ODS 表格 {}", path.display()),
            )
            .await
        }
        ("csv", OfficeContent::Table(rows)) => {
            let content = rows
                .iter()
                .map(|row| csv_line(row))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            write_text_file_with_confirmation(
                tool_name,
                path,
                content,
                context,
                format!("写 CSV 表格 {}", path.display()),
            )
            .await
        }
        ("tsv" | "xls", OfficeContent::Table(rows)) => {
            let content = rows
                .iter()
                .map(|row| tsv_line(row))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            write_text_file_with_confirmation(
                tool_name,
                path,
                content,
                context,
                format!("写表格 {}", path.display()),
            )
            .await
        }
        (other, _) => anyhow::bail!("内容类型与格式不匹配: {other}"),
    }
}

fn build_ods(rows: &[Vec<String>]) -> Result<Vec<u8>> {
    zip_package(vec![
        ZipEntry::stored(
            "mimetype",
            "application/vnd.oasis.opendocument.spreadsheet"
                .as_bytes()
                .to_vec(),
        ),
        ZipEntry::deflated("content.xml", build_ods_content_xml(rows).into_bytes()),
        ZipEntry::deflated("styles.xml", ODS_STYLES_XML.as_bytes().to_vec()),
        ZipEntry::deflated(
            "META-INF/manifest.xml",
            ODS_MANIFEST_XML.as_bytes().to_vec(),
        ),
    ])
}

fn build_odt(document: &DocumentData) -> Result<Vec<u8>> {
    zip_package(vec![
        ZipEntry::stored(
            "mimetype",
            "application/vnd.oasis.opendocument.text"
                .as_bytes()
                .to_vec(),
        ),
        ZipEntry::deflated("content.xml", build_odt_content_xml(document).into_bytes()),
        ZipEntry::deflated("styles.xml", ODS_STYLES_XML.as_bytes().to_vec()),
        ZipEntry::deflated(
            "META-INF/manifest.xml",
            ODT_MANIFEST_XML.as_bytes().to_vec(),
        ),
    ])
}

fn build_docx(document: &DocumentData, template: bool) -> Result<Vec<u8>> {
    let content_type = if template {
        DOCX_TEMPLATE_CONTENT_TYPES
    } else {
        DOCX_CONTENT_TYPES
    };
    zip_package(vec![
        ZipEntry::deflated("[Content_Types].xml", content_type.as_bytes().to_vec()),
        ZipEntry::deflated("_rels/.rels", DOCX_RELS.as_bytes().to_vec()),
        ZipEntry::deflated(
            "word/document.xml",
            build_docx_document_xml(document).into_bytes(),
        ),
    ])
}

fn build_pptx(presentation: &PresentationData, template: bool) -> Result<Vec<u8>> {
    let content_type = if template {
        PPTX_TEMPLATE_CONTENT_TYPES
    } else {
        PPTX_CONTENT_TYPES
    };
    let mut slides = Vec::with_capacity(presentation.slides.len() + 1);
    slides.push(SlideData {
        title: presentation.title.clone(),
        bullets: vec![format!("目标受众：{}", presentation.audience)],
        notes: String::new(),
    });
    slides.extend(presentation.slides.clone());
    let packaged = PresentationData {
        title: presentation.title.clone(),
        audience: presentation.audience.clone(),
        slides,
    };
    let mut entries = vec![
        ZipEntry::deflated("[Content_Types].xml", content_type.as_bytes().to_vec()),
        ZipEntry::deflated("_rels/.rels", PPTX_RELS.as_bytes().to_vec()),
        ZipEntry::deflated(
            "ppt/presentation.xml",
            build_presentation_xml(&packaged).into_bytes(),
        ),
        ZipEntry::deflated(
            "ppt/_rels/presentation.xml.rels",
            build_presentation_rels(packaged.slides.len()).into_bytes(),
        ),
    ];
    for (index, slide) in packaged.slides.iter().enumerate() {
        entries.push(ZipEntry::deflated(
            format!("ppt/slides/slide{}.xml", index + 1),
            build_slide_xml(slide).into_bytes(),
        ));
    }
    zip_package(entries)
}

fn build_xlsx(rows: &[Vec<String>], template: bool) -> Result<Vec<u8>> {
    let content_type = if template {
        XLSX_TEMPLATE_CONTENT_TYPES
    } else {
        XLSX_CONTENT_TYPES
    };
    zip_package(vec![
        ZipEntry::deflated("[Content_Types].xml", content_type.as_bytes().to_vec()),
        ZipEntry::deflated("_rels/.rels", XLSX_RELS.as_bytes().to_vec()),
        ZipEntry::deflated("xl/workbook.xml", XLSX_WORKBOOK.as_bytes().to_vec()),
        ZipEntry::deflated(
            "xl/_rels/workbook.xml.rels",
            XLSX_WORKBOOK_RELS.as_bytes().to_vec(),
        ),
        ZipEntry::deflated(
            "xl/worksheets/sheet1.xml",
            build_sheet_xml(rows).into_bytes(),
        ),
    ])
}

fn build_epub(document: &DocumentData) -> Result<Vec<u8>> {
    zip_package(vec![
        ZipEntry::stored("mimetype", "application/epub+zip".as_bytes().to_vec()),
        ZipEntry::deflated(
            "META-INF/container.xml",
            EPUB_CONTAINER_XML.as_bytes().to_vec(),
        ),
        ZipEntry::deflated("OEBPS/content.opf", build_epub_opf(document).into_bytes()),
        ZipEntry::deflated("OEBPS/toc.ncx", build_epub_toc(document).into_bytes()),
        ZipEntry::deflated(
            "OEBPS/chapter.xhtml",
            build_epub_chapter(document).into_bytes(),
        ),
    ])
}

fn build_rtf(document: &DocumentData) -> String {
    let mut rtf = String::from("{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Arial;}}\n");
    rtf.push_str(&format!(
        "\\b\\fs32 {}\\b0\\fs24\\par\n",
        escape_rtf(&document.title)
    ));
    for section in &document.sections {
        rtf.push_str(&format!("\\b {}\\b0\\par\n", escape_rtf(&section.heading)));
        rtf.push_str(&format!("{}\\par\n", escape_rtf(&section.body)));
    }
    rtf.push('}');
    rtf
}

fn build_pdf(document: &DocumentData) -> Vec<u8> {
    let mut lines = vec![document.title.clone()];
    for section in &document.sections {
        lines.push(section.heading.clone());
        lines.extend(section.body.lines().map(ToString::to_string));
    }
    let mut stream = String::from("BT /F1 12 Tf 50 780 Td 14 TL ");
    for line in lines {
        stream.push_str(&format!("({}) Tj T* ", escape_pdf_text(&line)));
    }
    stream.push_str("ET");
    let objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{}\nendstream", stream.len(), stream),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{}\nendobj\n", index + 1, object));
    }
    let xref = pdf.len();
    pdf.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        objects.len() + 1
    ));
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer << /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
        objects.len() + 1,
        xref
    ));
    pdf.into_bytes()
}

struct ZipEntry {
    path: String,
    content: Vec<u8>,
    stored: bool,
}

impl ZipEntry {
    fn stored(path: impl Into<String>, content: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            content,
            stored: true,
        }
    }

    fn deflated(path: impl Into<String>, content: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            content,
            stored: false,
        }
    }
}

fn zip_package(entries: Vec<ZipEntry>) -> Result<Vec<u8>> {
    let mut buffer = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut buffer);
    for entry in entries {
        let options =
            zip::write::SimpleFileOptions::default().compression_method(if entry.stored {
                zip::CompressionMethod::Stored
            } else {
                zip::CompressionMethod::Deflated
            });
        writer.start_file(entry.path, options)?;
        writer.write_all(&entry.content)?;
    }
    writer.finish()?;
    Ok(buffer.into_inner())
}

fn build_ods_content_xml(rows: &[Vec<String>]) -> String {
    let mut xml = String::from(ODS_CONTENT_PREFIX);
    for row in rows {
        xml.push_str("<table:table-row>");
        for cell in row {
            xml.push_str("<table:table-cell office:value-type=\"string\"><text:p>");
            xml.push_str(&escape_xml(cell));
            xml.push_str("</text:p></table:table-cell>");
        }
        xml.push_str("</table:table-row>");
    }
    xml.push_str(ODS_CONTENT_SUFFIX);
    xml
}

fn build_odt_content_xml(document: &DocumentData) -> String {
    let mut xml = String::from(ODT_CONTENT_PREFIX);
    xml.push_str(&format!(
        "<text:h text:outline-level=\"1\">{}</text:h>",
        escape_xml(&document.title)
    ));
    for section in &document.sections {
        xml.push_str(&format!(
            "<text:h text:outline-level=\"2\">{}</text:h><text:p>{}</text:p>",
            escape_xml(&section.heading),
            escape_xml(&section.body)
        ));
    }
    xml.push_str(ODT_CONTENT_SUFFIX);
    xml
}

fn build_docx_document_xml(document: &DocumentData) -> String {
    let mut xml = String::from(DOCX_DOCUMENT_PREFIX);
    xml.push_str(&docx_paragraph(&document.title, true));
    for section in &document.sections {
        xml.push_str(&docx_paragraph(&section.heading, true));
        xml.push_str(&docx_paragraph(&section.body, false));
    }
    xml.push_str(DOCX_DOCUMENT_SUFFIX);
    xml
}

fn docx_paragraph(text: &str, bold: bool) -> String {
    let bold_tag = if bold { "<w:rPr><w:b/></w:rPr>" } else { "" };
    format!(
        "<w:p><w:r>{}<w:t>{}</w:t></w:r></w:p>",
        bold_tag,
        escape_xml(text)
    )
}

fn build_presentation_xml(presentation: &PresentationData) -> String {
    let slides = (1..=presentation.slides.len())
        .map(|index| format!("<p:sldId id=\"{}\" r:id=\"rId{}\"/>", 255 + index, index))
        .collect::<Vec<_>>()
        .join("");
    format!(
        "{}<p:sldIdLst>{}</p:sldIdLst><p:sldSz cx=\"9144000\" cy=\"5143500\" type=\"screen16x9\"/></p:presentation>",
        PPTX_PRESENTATION_PREFIX, slides
    )
}

fn build_presentation_rels(slide_count: usize) -> String {
    let relationships = (1..=slide_count)
        .map(|index| format!("<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"slides/slide{}.xml\"/>", index, index))
        .collect::<Vec<_>>()
        .join("");
    format!("{}{}{}", RELS_PREFIX, relationships, RELS_SUFFIX)
}

fn build_slide_xml(slide: &SlideData) -> String {
    let mut body = format!("{}\n{}", slide.title, slide.bullets.join("\n"));
    if !slide.notes.is_empty() {
        body.push('\n');
        body.push_str(&slide.notes);
    }
    PPTX_SLIDE_XML.replace("{content}", &escape_xml(&body))
}

fn build_sheet_xml(rows: &[Vec<String>]) -> String {
    let mut xml = String::from(XLSX_SHEET_PREFIX);
    for (row_index, row) in rows.iter().enumerate() {
        xml.push_str(&format!("<row r=\"{}\">", row_index + 1));
        for (col_index, cell) in row.iter().enumerate() {
            xml.push_str(&format!(
                "<c r=\"{}{}\" t=\"inlineStr\"><is><t>{}</t></is></c>",
                xlsx_column_name(col_index),
                row_index + 1,
                escape_xml(cell)
            ));
        }
        xml.push_str("</row>");
    }
    xml.push_str(XLSX_SHEET_SUFFIX);
    xml
}

fn xlsx_column_name(mut index: usize) -> String {
    let mut name = String::new();
    loop {
        name.insert(0, (b'A' + (index % 26) as u8) as char);
        if index < 26 {
            return name;
        }
        index = index / 26 - 1;
    }
}

fn build_epub_opf(document: &DocumentData) -> String {
    EPUB_OPF_XML.replace("{title}", &escape_xml(&document.title))
}

fn build_epub_toc(document: &DocumentData) -> String {
    EPUB_TOC_XML.replace("{title}", &escape_xml(&document.title))
}

fn build_epub_chapter(document: &DocumentData) -> String {
    let mut body = format!("<h1>{}</h1>", escape_xml(&document.title));
    for section in &document.sections {
        body.push_str(&format!(
            "<h2>{}</h2><p>{}</p>",
            escape_xml(&section.heading),
            escape_xml(&section.body)
        ));
    }
    EPUB_CHAPTER_XML.replace("{body}", &body)
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn escape_rtf(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('{', "\\{")
        .replace('}', "\\}")
        .replace('\n', "\\line ")
}

fn escape_pdf_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .replace('\r', " ")
        .replace('\n', " ")
}

fn tsv_line(values: &[String]) -> String {
    values
        .iter()
        .map(|value| value.replace('\t', " ").replace('\n', " "))
        .collect::<Vec<_>>()
        .join("\t")
}

const ODS_CONTENT_PREFIX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2">
<office:body><office:spreadsheet><table:table table:name="Sheet1">"#;

const ODS_CONTENT_SUFFIX: &str =
    r#"</table:table></office:spreadsheet></office:body></office:document-content>"#;

const ODS_STYLES_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" office:version="1.2"/>"#;

const ODS_MANIFEST_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2">
<manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
<manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>
</manifest:manifest>"#;

const ODT_CONTENT_PREFIX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2">
<office:body><office:text>"#;

const ODT_CONTENT_SUFFIX: &str = r#"</office:text></office:body></office:document-content>"#;

const ODT_MANIFEST_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2">
<manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
<manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>
</manifest:manifest>"#;

const RELS_PREFIX: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#;
const RELS_SUFFIX: &str = r#"</Relationships>"#;

const DOCX_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
const DOCX_TEMPLATE_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.template.main+xml"/></Types>"#;
const DOCX_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
const DOCX_DOCUMENT_PREFIX: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#;
const DOCX_DOCUMENT_SUFFIX: &str = r#"<w:sectPr/></w:body></w:document>"#;

const PPTX_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/></Types>"#;
const PPTX_TEMPLATE_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.template.main+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/></Types>"#;
const PPTX_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#;
const PPTX_PRESENTATION_PREFIX: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#;
const PPTX_SLIDE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:sp><p:nvSpPr><p:cNvPr id="2" name="Content"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>{content}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;

const XLSX_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;
const XLSX_TEMPLATE_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.template.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;
const XLSX_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
const XLSX_WORKBOOK: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
const XLSX_WORKBOOK_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;
const XLSX_SHEET_PREFIX: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#;
const XLSX_SHEET_SUFFIX: &str = r#"</sheetData></worksheet>"#;

const EPUB_CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
const EPUB_OPF_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><package version="2.0" unique-identifier="bookid" xmlns="http://www.idpf.org/2007/opf"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{title}</dc:title><dc:language>zh-CN</dc:language><dc:identifier id="bookid">urn:uuid:yunzhi-one-cli</dc:identifier></metadata><manifest><item id="toc" href="toc.ncx" media-type="application/x-dtbncx+xml"/><item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/></manifest><spine toc="toc"><itemref idref="chapter"/></spine></package>"#;
const EPUB_TOC_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><ncx version="2005-1" xmlns="http://www.daisy.org/z3986/2005/ncx/"><head/><docTitle><text>{title}</text></docTitle><navMap><navPoint id="chapter" playOrder="1"><navLabel><text>正文</text></navLabel><content src="chapter.xhtml"/></navPoint></navMap></ncx>"#;
const EPUB_CHAPTER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><html xmlns="http://www.w3.org/1999/xhtml"><head><title>正文</title></head><body>{body}</body></html>"#;

fn csv_line(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("\"{}\"", value.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(",")
}

fn strip_html(raw: &str) -> String {
    let mut output = String::new();
    let mut in_tag = false;
    for char in raw.chars() {
        match char {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                output.push(' ');
            }
            _ if !in_tag => output.push(char),
            _ => {}
        }
    }
    output
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn percent_encode_query(query: &str) -> String {
    let mut encoded = String::new();
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            b' ' => encoded.push('+'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn http_client(timeout_secs: u64) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .context("创建 HTTP 客户端失败")
}

fn render_process_output(
    command: String,
    output: std::process::Output,
    timeout_secs: u64,
) -> ToolOutput {
    let mut rendered = format!(
        "command: {}\ntimeout: {}s\nexit: {}\n",
        command, timeout_secs, output.status
    );
    if !output.stdout.is_empty() {
        rendered.push_str("stdout:\n");
        rendered.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        rendered.push_str("\nstderr:\n");
        rendered.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    ToolOutput {
        content: rendered,
        is_error: !output.status.success(),
    }
}

async fn copy_path(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::metadata(source)
        .await
        .with_context(|| format!("读取源路径失败: {}", source.display()))?;
    if metadata.is_dir() {
        copy_dir_recursive(source, destination).with_context(|| {
            format!(
                "复制目录失败: {} -> {}",
                source.display(),
                destination.display()
            )
        })
    } else {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::copy(source, destination).await.with_context(|| {
            format!(
                "复制文件失败: {} -> {}",
                source.display(),
                destination.display()
            )
        })?;
        Ok(())
    }
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = destination_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

async fn run_command(command: Vec<String>, cwd: &Path, timeout_secs: u64) -> Result<ToolOutput> {
    run_command_with_sandbox(command, cwd, timeout_secs, false).await
}

async fn run_shell_command(
    command: &str,
    cwd: &Path,
    timeout_secs: u64,
    sandbox: bool,
) -> Result<ToolOutput> {
    run_command_with_sandbox(
        vec!["sh".to_string(), "-c".to_string(), command.to_string()],
        cwd,
        timeout_secs,
        sandbox,
    )
    .await
}

async fn run_command_with_sandbox(
    command: Vec<String>,
    cwd: &Path,
    timeout_secs: u64,
    sandbox: bool,
) -> Result<ToolOutput> {
    anyhow::ensure!(!command.is_empty(), "命令不能为空");
    if sandbox {
        let sandbox_dir = tempfile::Builder::new()
            .prefix("yunzhi-sandbox-")
            .tempdir()
            .context("创建沙箱目录失败")?;
        let workspace = sandbox_dir.path().join("workspace");
        copy_workspace_for_sandbox(cwd, &workspace)?;
        let mut output = run_command_in_dir(command, &workspace, timeout_secs).await?;
        output.content = format!(
            "sandbox: {}\n{}",
            workspace.display(),
            output.content.trim_start()
        );
        return Ok(output);
    }
    run_command_in_dir(command, cwd, timeout_secs).await
}

async fn run_command_in_dir(
    command: Vec<String>,
    cwd: &Path,
    timeout_secs: u64,
) -> Result<ToolOutput> {
    let started = Instant::now();
    let mut process = Command::new(&command[0]);
    process.args(&command[1..]).current_dir(cwd);
    let output = timeout(Duration::from_secs(timeout_secs), process.output())
        .await
        .map_err(|_| {
            anyhow!(
                "命令超时，已终止: {}",
                shell_words::join(command.iter().map(String::as_str))
            )
        })??;
    let elapsed = started.elapsed().as_secs_f32();
    let mut rendered = format!(
        "command: {}\nexit: {} ({elapsed:.1}s)\n",
        shell_words::join(command.iter().map(String::as_str)),
        output.status
    );
    if !output.stdout.is_empty() {
        rendered.push_str("stdout:\n");
        rendered.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        rendered.push_str("\nstderr:\n");
        rendered.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ok(ToolOutput {
        content: rendered,
        is_error: !output.status.success(),
    })
}

fn copy_workspace_for_sandbox(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    for path in searchable_files(source, source) {
        let rel = path.strip_prefix(source).unwrap_or(&path);
        let target = destination.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&path, &target).with_context(|| {
            format!(
                "复制沙箱文件失败: {} -> {}",
                path.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

async fn confirm_git_action(
    context: &mut ToolContext,
    tool_name: &str,
    summary: String,
) -> Result<PermissionDecision> {
    context
        .confirm(PermissionRequest {
            tool_name: tool_name.to_string(),
            summary,
            diff: None,
        })
        .await
}

async fn git_status(cwd: &Path, paths: &[String], timeout_secs: u64) -> Result<ToolOutput> {
    let mut command = vec![
        "git".to_string(),
        "status".to_string(),
        "--short".to_string(),
    ];
    append_git_paths(&mut command, paths);
    let output = run_command(command, cwd, timeout_secs).await?;
    if output.content.trim().is_empty() {
        return Ok(ToolOutput::ok("工作区干净"));
    }
    Ok(output)
}

async fn git_diff_tool(
    cwd: &Path,
    paths: &[String],
    staged: bool,
    timeout_secs: u64,
) -> Result<ToolOutput> {
    let mut command = vec!["git".to_string(), "diff".to_string()];
    if staged {
        command.push("--staged".to_string());
    }
    append_git_paths(&mut command, paths);
    run_command(command, cwd, timeout_secs).await
}

async fn git_review_diff(
    cwd: &Path,
    paths: &[String],
    staged: bool,
    timeout_secs: u64,
) -> Result<ToolOutput> {
    let status = git_status(cwd, paths, timeout_secs).await?;
    let diff = git_diff_tool(cwd, paths, staged, timeout_secs).await?;
    Ok(ToolOutput {
        content: format!(
            "code_review_context:\nstatus:\n{}\n\ndiff:\n{}",
            status.content,
            truncate_text(&diff.content, 30_000)
        ),
        is_error: status.is_error || diff.is_error,
    })
}

async fn git_message(cwd: &Path, paths: &[String], timeout_secs: u64) -> Result<ToolOutput> {
    let message = generate_commit_message(cwd, paths, timeout_secs).await?;
    Ok(ToolOutput::ok(message))
}

async fn generate_commit_message(
    cwd: &Path,
    paths: &[String],
    timeout_secs: u64,
) -> Result<String> {
    let mut command = vec![
        "git".to_string(),
        "status".to_string(),
        "--short".to_string(),
    ];
    append_git_paths(&mut command, paths);
    let output = run_command(command, cwd, timeout_secs).await?;
    let mut added = 0usize;
    let mut modified = 0usize;
    let mut deleted = 0usize;
    let mut renamed = 0usize;
    let mut files = Vec::new();
    for status_line in git_status_lines(&output.content) {
        let code = &status_line[..2];
        let file = status_line[3..].trim().to_string();
        if code.contains('A') || code.contains('?') {
            added += 1;
        } else if code.contains('D') {
            deleted += 1;
        } else if code.contains('R') {
            renamed += 1;
        } else {
            modified += 1;
        }
        files.push(file);
    }
    if files.is_empty() {
        return Ok("Update project files".to_string());
    }
    let verb = if added > 0 && modified == 0 && deleted == 0 && renamed == 0 {
        "Add"
    } else if deleted > 0 && added == 0 && modified == 0 && renamed == 0 {
        "Remove"
    } else if renamed > 0 && added == 0 && modified == 0 && deleted == 0 {
        "Rename"
    } else {
        "Update"
    };
    let subject = if files.len() == 1 {
        humanize_file_for_commit(&files[0])
    } else {
        format!("{} files", files.len())
    };
    Ok(format!("{verb} {subject}"))
}

async fn git_add(cwd: &Path, paths: &[String], timeout_secs: u64) -> Result<ToolOutput> {
    let mut command = vec!["git".to_string(), "add".to_string()];
    if paths.is_empty() {
        command.push("-A".to_string());
    } else {
        append_git_paths(&mut command, paths);
    }
    run_command(command, cwd, timeout_secs).await
}

fn append_git_paths(command: &mut Vec<String>, paths: &[String]) {
    if !paths.is_empty() {
        command.push("--".to_string());
        command.extend(paths.iter().cloned());
    }
}

fn humanize_file_for_commit(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(path)
        .replace(['_', '-'], " ")
}

fn detect_test_command(cwd: &Path) -> Result<String> {
    if cwd.join("Cargo.toml").exists() {
        return Ok("cargo test".to_string());
    }
    if cwd.join("package.json").exists() {
        return Ok("npm test".to_string());
    }
    if cwd.join("pnpm-lock.yaml").exists() {
        return Ok("pnpm test".to_string());
    }
    if cwd.join("yarn.lock").exists() {
        return Ok("yarn test".to_string());
    }
    if cwd.join("pyproject.toml").exists()
        || cwd.join("pytest.ini").exists()
        || cwd.join("tox.ini").exists()
    {
        return Ok("pytest".to_string());
    }
    if cwd.join("go.mod").exists() {
        return Ok("go test ./...".to_string());
    }
    anyhow::bail!("无法自动探测测试命令，请提供 command 参数")
}

fn format_test_output(output: &str) -> String {
    let failure_lines = output
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("failed")
                || lower.contains("error")
                || lower.contains("panic")
                || lower.contains("assert")
                || lower.contains("failure")
                || lower.contains("test result")
                || lower.contains("running")
                || lower.contains("exit:")
        })
        .take(120)
        .collect::<Vec<_>>();
    if failure_lines.is_empty() {
        truncate_text(output, 12_000)
    } else {
        failure_lines.join("\n")
    }
}

fn git_status_lines(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter(|line| {
            line.len() >= 4
                && !line.starts_with("command:")
                && !line.starts_with("exit:")
                && !line.starts_with("stdout:")
                && !line.starts_with("stderr:")
        })
        .collect()
}

fn parse_todo_status(raw: &str) -> Result<TodoStatus> {
    match raw {
        "pending" => Ok(TodoStatus::Pending),
        "in_progress" => Ok(TodoStatus::InProgress),
        "done" => Ok(TodoStatus::Done),
        "blocked" => Ok(TodoStatus::Blocked),
        status => anyhow::bail!("不支持的任务状态: {status}"),
    }
}

fn render_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "暂无代办任务".to_string();
    }
    todos
        .iter()
        .map(|item| {
            let notes = item.notes.as_deref().unwrap_or("");
            if notes.is_empty() {
                format!("{} [{:?}] {}", item.id, item.status, item.title)
            } else {
                format!("{} [{:?}] {} - {}", item.id, item.status, item.title, notes)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct AlwaysAllowPrompter;

#[async_trait]
impl PermissionPrompter for AlwaysAllowPrompter {
    async fn confirm(&self, _request: PermissionRequest) -> Result<PermissionDecision> {
        Ok(PermissionDecision::Allow)
    }

    async fn ask_user(&self, request: UserQuestionRequest) -> Result<String> {
        Ok(request.default_answer.unwrap_or_default())
    }

    async fn choose_option(&self, request: UserChoiceRequest) -> Result<UserChoiceResponse> {
        let answer = request
            .options
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("options 不能为空"))?;
        Ok(UserChoiceResponse {
            answer,
            index: Some(0),
            custom: false,
        })
    }
}

#[cfg(test)]
struct FixedUserPrompter;

#[cfg(test)]
#[async_trait]
impl PermissionPrompter for FixedUserPrompter {
    async fn confirm(&self, _request: PermissionRequest) -> Result<PermissionDecision> {
        Ok(PermissionDecision::Allow)
    }

    async fn ask_user(&self, _request: UserQuestionRequest) -> Result<String> {
        Ok("用户回答".to_string())
    }

    async fn choose_option(&self, request: UserChoiceRequest) -> Result<UserChoiceResponse> {
        Ok(UserChoiceResponse {
            answer: request.options[1].clone(),
            index: Some(1),
            custom: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::tempdir;

    fn context(dir: &Path) -> ToolContext {
        ToolContext::new(
            dir.to_path_buf(),
            "test-key".to_string(),
            true,
            Arc::new(AlwaysAllowPrompter),
            false,
        )
    }

    fn fixed_user_context(dir: &Path) -> ToolContext {
        ToolContext::new(
            dir.to_path_buf(),
            "test-key".to_string(),
            true,
            Arc::new(FixedUserPrompter),
            false,
        )
    }

    #[tokio::test]
    async fn reads_and_edits_file() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        registry
            .execute(
                "write_file",
                json!({"path":"a.txt","content":"hello world"}),
                &mut ctx,
            )
            .await;
        let output = registry
            .execute(
                "edit_file",
                json!({"path":"a.txt","old_str":"world","new_str":"yunzhi"}),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let output = registry
            .execute("read_file", json!({"path":"a.txt"}), &mut ctx)
            .await;
        assert_eq!(output.content, "hello yunzhi");
    }

    #[tokio::test]
    async fn rejects_ambiguous_edit() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        registry
            .execute(
                "write_file",
                json!({"path":"a.txt","content":"x x"}),
                &mut ctx,
            )
            .await;
        let output = registry
            .execute(
                "edit_file",
                json!({"path":"a.txt","old_str":"x","new_str":"y"}),
                &mut ctx,
            )
            .await;
        assert!(output.is_error);
    }

    #[test]
    fn applies_selected_diff_hunks_only() {
        let old = "one\na\nb\nc\nd\ne\nf\ng\nthree\n";
        let new = "ONE\na\nb\nc\nd\ne\nf\ng\nTHREE\n";
        let hunks = diff_hunks(old, new);
        assert_eq!(hunks.len(), 2);
        let applied = apply_selected_hunks(old, &hunks, &[2]);
        assert_eq!(applied, "one\na\nb\nc\nd\ne\nf\ng\nTHREE\n");
    }

    #[tokio::test]
    async fn search_respects_gitignore_and_default_ignored_dirs() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "needle\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "needle\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/generated.txt"), "needle\n").unwrap();

        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute("grep_search", json!({"pattern":"needle"}), &mut ctx)
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("visible.txt"));
        assert!(!output.content.contains("ignored.txt"));
        assert!(!output.content.contains("target/generated.txt"));
    }

    #[tokio::test]
    async fn code_index_reports_symbols_and_references() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub struct Agent {}\nfn run_agent() {}\nfn other() { run_agent(); }\n",
        )
        .unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute("code_index", json!({"query":"run_agent"}), &mut ctx)
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("function:run_agent:2"));
        assert!(output.content.contains("ref\tlib.rs:3"));
    }

    #[tokio::test]
    async fn manages_todos() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute(
                "manage_todos",
                json!({"action":"add","title":"实现工具","status":"in_progress"}),
                &mut ctx,
            )
            .await;
        assert!(output.content.contains("实现工具"));
        assert!(output.content.contains("InProgress"));
        let output = registry
            .execute(
                "manage_todos",
                json!({"action":"update","id":1,"status":"done"}),
                &mut ctx,
            )
            .await;
        assert!(output.content.contains("Done"));
    }

    #[tokio::test]
    async fn asks_user_for_free_text() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = fixed_user_context(dir.path());
        let output = registry
            .execute(
                "ask_user",
                json!({"question":"目标文件名是什么？","context":"需要确定写入路径"}),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content, "用户回答");
    }

    #[tokio::test]
    async fn chooses_user_option() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = fixed_user_context(dir.path());
        let output = registry
            .execute(
                "choose_option",
                json!({"question":"选择模式","options":["chat","agent","team"]}),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("agent"));
        assert!(output.content.contains("\"index\": 1"));
        assert!(output.content.contains("\"custom\": false"));
    }

    #[tokio::test]
    async fn choose_option_requires_two_options() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = fixed_user_context(dir.path());
        let output = registry
            .execute(
                "choose_option",
                json!({"question":"选择模式","options":["agent"]}),
                &mut ctx,
            )
            .await;
        assert!(output.is_error);
    }

    #[tokio::test]
    async fn executes_code() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute(
                "execute_code",
                json!({"language":"bash","code":"printf yunzhi"}),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("yunzhi"));
        assert!(output.content.contains("sandbox:"));
    }

    #[tokio::test]
    async fn bash_runs_in_sandbox_by_default() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute(
                "bash",
                json!({"command":"printf changed > original.txt"}),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("sandbox:"));
        assert!(!dir.path().join("original.txt").exists());
    }

    #[tokio::test]
    async fn bash_can_run_in_workspace_when_sandbox_disabled() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute(
                "bash",
                json!({"command":"printf changed > original.txt","sandbox":false}),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("original.txt")).unwrap(),
            "changed"
        );
    }

    #[tokio::test]
    async fn git_manager_reports_status_diff_and_message() {
        let dir = tempdir().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("feature.rs"), "fn old() {}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "feature.rs"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "Add feature"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("feature.rs"), "fn new() {}\n").unwrap();

        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let status = registry
            .execute("git_manager", json!({"action":"status"}), &mut ctx)
            .await;
        assert!(!status.is_error, "{}", status.content);
        assert!(status.content.contains("feature.rs"));

        let diff = registry
            .execute("git_manager", json!({"action":"diff"}), &mut ctx)
            .await;
        assert!(!diff.is_error, "{}", diff.content);
        assert!(diff.content.contains("fn old"));

        let message = registry
            .execute("git_manager", json!({"action":"message"}), &mut ctx)
            .await;
        assert!(!message.is_error, "{}", message.content);
        assert_eq!(message.content, "Update feature");
    }

    #[test]
    fn detects_cargo_test_command() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        assert_eq!(detect_test_command(dir.path()).unwrap(), "cargo test");
    }

    #[tokio::test]
    async fn test_loop_reports_failed_command() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute(
                "test_loop",
                json!({"command":"printf 'error: boom\n' >&2; exit 1","timeout":5}),
                &mut ctx,
            )
            .await;
        assert!(output.is_error);
        assert!(output.content.contains("test_status: failed"));
        assert!(output.content.contains("error: boom"));
    }

    #[tokio::test]
    async fn system_control_reports_pwd() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute("system_control", json!({"action":"pwd"}), &mut ctx)
            .await;
        assert_eq!(output.content, dir.path().display().to_string());
    }

    #[tokio::test]
    async fn lists_and_reads_skills() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join(".yunzhi/skills/review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: Review helper\n---\n# Review\nUse care.",
        )
        .unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());

        let output = registry.execute("list_skills", json!({}), &mut ctx).await;
        assert!(output.content.contains("review"));
        assert!(output.content.contains("Review helper"));

        let output = registry
            .execute("read_skill", json!({"skill":"review"}), &mut ctx)
            .await;
        assert!(output.content.contains("Use care."));
    }

    #[tokio::test]
    async fn lists_mcp_servers() {
        let dir = tempdir().unwrap();
        let config_dir = dir.path().join(".yunzhi");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("mcp.json"),
            r#"{"servers":{"demo":{"command":"echo","args":["ok"]}}}"#,
        )
        .unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());

        let output = registry
            .execute("list_mcp_servers", json!({}), &mut ctx)
            .await;
        assert!(output.content.contains("demo"));
        assert!(output.content.contains("echo ok"));
    }

    #[tokio::test]
    async fn appends_and_reports_file_info() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        registry
            .execute(
                "write_file",
                json!({"path":"notes.txt","content":"hello"}),
                &mut ctx,
            )
            .await;
        let output = registry
            .execute(
                "append_file",
                json!({"path":"notes.txt","content":" world"}),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let output = registry
            .execute("read_file", json!({"path":"notes.txt"}), &mut ctx)
            .await;
        assert_eq!(output.content, "hello world");
        let output = registry
            .execute("file_info", json!({"path":"notes.txt"}), &mut ctx)
            .await;
        assert!(output.content.contains("type: file"));
        assert!(output.content.contains("size: 11"));
    }

    #[tokio::test]
    async fn creates_copies_moves_and_deletes_paths() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute("create_dir", json!({"path":"a/b"}), &mut ctx)
            .await;
        assert!(!output.is_error, "{}", output.content);
        registry
            .execute(
                "write_file",
                json!({"path":"a/b/source.txt","content":"copy me"}),
                &mut ctx,
            )
            .await;
        let output = registry
            .execute(
                "copy_path",
                json!({"source":"a","destination":"copied"}),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let output = registry
            .execute("read_file", json!({"path":"copied/b/source.txt"}), &mut ctx)
            .await;
        assert_eq!(output.content, "copy me");
        let output = registry
            .execute(
                "move_path",
                json!({"source":"copied/b/source.txt","destination":"moved.txt"}),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let output = registry
            .execute("read_file", json!({"path":"moved.txt"}), &mut ctx)
            .await;
        assert_eq!(output.content, "copy me");
        let output = registry
            .execute(
                "delete_path",
                json!({"path":"copied","recursive":true}),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(!dir.path().join("copied").exists());
    }

    #[tokio::test]
    async fn refuses_non_recursive_directory_delete() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        registry
            .execute("create_dir", json!({"path":"nested"}), &mut ctx)
            .await;
        let output = registry
            .execute("delete_path", json!({"path":"nested"}), &mut ctx)
            .await;
        assert!(output.is_error);
    }

    #[tokio::test]
    async fn creates_presentation_markdown() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute(
                "create_presentation",
                json!({
                    "path":"deck.md",
                    "title":"产品计划",
                    "audience":"研发团队",
                    "slides":[
                        {"title":"目标","bullets":["统一工具能力","降低手工操作"]},
                        {"title":"下一步","speaker_notes":"控制节奏"}
                    ]
                }),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let content = std::fs::read_to_string(dir.path().join("deck.md")).unwrap();
        assert!(content.contains("marp: true"));
        assert!(content.contains("# 产品计划"));
        assert!(content.contains("- 统一工具能力"));
    }

    #[tokio::test]
    async fn writes_csv_table() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute(
                "write_table",
                json!({
                    "path":"table.csv",
                    "format":"csv",
                    "columns":["name","note"],
                    "rows":[["yunzhi","hello, world"],["quote","a \" b"]]
                }),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let content = std::fs::read_to_string(dir.path().join("table.csv")).unwrap();
        assert!(content.contains("\"name\",\"note\""));
        assert!(content.contains("\"yunzhi\",\"hello, world\""));
        assert!(content.contains("\"quote\",\"a \"\" b\""));
    }

    #[tokio::test]
    async fn writes_calc_ods_table() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute(
                "write_table",
                json!({
                    "path":"table.ods",
                    "format":"calc",
                    "columns":["name","note"],
                    "rows":[["yunzhi","A & B"],["tag","<ok>"]]
                }),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);

        let file = std::fs::File::open(dir.path().join("table.ods")).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut content_xml = String::new();
        archive
            .by_name("content.xml")
            .unwrap()
            .read_to_string(&mut content_xml)
            .unwrap();
        assert!(content_xml.contains("table:name=\"Sheet1\""));
        assert!(content_xml.contains("yunzhi"));
        assert!(content_xml.contains("A &amp; B"));
        assert!(content_xml.contains("&lt;ok&gt;"));
    }

    #[tokio::test]
    async fn office_document_writes_representative_formats() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());

        let docx = registry
            .execute(
                "office_document",
                json!({
                    "path":"report.docx",
                    "format":"word",
                    "title":"报告",
                    "sections":[{"heading":"摘要","body":"hello"}]
                }),
                &mut ctx,
            )
            .await;
        assert!(!docx.is_error, "{}", docx.content);
        let file = std::fs::File::open(dir.path().join("report.docx")).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut document_xml = String::new();
        archive
            .by_name("word/document.xml")
            .unwrap()
            .read_to_string(&mut document_xml)
            .unwrap();
        assert!(document_xml.contains("hello"));

        let xlsx = registry
            .execute(
                "office_document",
                json!({
                    "path":"data.xlsx",
                    "format":"excel",
                    "columns":["name","value"],
                    "rows":[["yunzhi","1"]]
                }),
                &mut ctx,
            )
            .await;
        assert!(!xlsx.is_error, "{}", xlsx.content);
        let file = std::fs::File::open(dir.path().join("data.xlsx")).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut sheet_xml = String::new();
        archive
            .by_name("xl/worksheets/sheet1.xml")
            .unwrap()
            .read_to_string(&mut sheet_xml)
            .unwrap();
        assert!(sheet_xml.contains("yunzhi"));

        let pptx = registry
            .execute(
                "office_document",
                json!({
                    "path":"deck.pptx",
                    "format":"ppt",
                    "title":"演示",
                    "slides":[{"title":"第一页","bullets":["要点"]}]
                }),
                &mut ctx,
            )
            .await;
        assert!(!pptx.is_error, "{}", pptx.content);
        let file = std::fs::File::open(dir.path().join("deck.pptx")).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        assert!(archive.by_name("ppt/slides/slide1.xml").is_ok());
        assert!(archive.by_name("ppt/slides/slide2.xml").is_ok());

        let epub = registry
            .execute(
                "office_document",
                json!({
                    "path":"book.epub",
                    "format":"epub",
                    "title":"电子书",
                    "sections":[{"heading":"章","body":"内容"}]
                }),
                &mut ctx,
            )
            .await;
        assert!(!epub.is_error, "{}", epub.content);
        let file = std::fs::File::open(dir.path().join("book.epub")).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        assert!(archive.by_name("OEBPS/chapter.xhtml").is_ok());
    }

    #[tokio::test]
    async fn manages_long_memory_file() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::builtin();
        let mut ctx = context(dir.path());
        let output = registry
            .execute(
                "long_memory",
                json!({"action":"append","content":"- 记住项目约定"}),
                &mut ctx,
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        let output = registry
            .execute("long_memory", json!({"action":"read"}), &mut ctx)
            .await;
        assert!(output.content.contains("记住项目约定"));
    }

    #[test]
    fn encodes_query_and_strips_html() {
        assert_eq!(percent_encode_query("a b/c?"), "a+b%2Fc%3F");
        assert_eq!(
            strip_html("<h1>A&nbsp;&amp;&quot;B&quot;</h1>\n<p>C</p>"),
            "A &\"B\" C"
        );
    }
}
