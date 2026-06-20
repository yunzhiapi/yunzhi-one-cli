use crate::llm::ChatCompletionsClient;
use crate::types::{ToolDefinition, ToolOutput};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use glob::glob;
use serde::{Deserialize, Serialize};
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
    pub api_key: String,
    pub dangerously_skip_permissions: bool,
    pub allow_all: bool,
    pub prompter: Arc<dyn PermissionPrompter>,
    todos: Vec<TodoItem>,
    next_todo_id: u64,
}

impl ToolContext {
    pub fn new(
        cwd: PathBuf,
        api_key: String,
        dangerously_skip_permissions: bool,
        prompter: Arc<dyn PermissionPrompter>,
    ) -> Self {
        Self {
            cwd,
            api_key,
            dangerously_skip_permissions,
            allow_all: false,
            prompter,
            todos: Vec::new(),
            next_todo_id: 1,
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
        registry.register(BashTool);
        registry.register(GlobSearchTool);
        registry.register(GrepSearchTool);
        registry.register(ListDirTool);
        registry.register(CallModelTool);
        registry.register(ExecuteCodeTool);
        registry.register(RunProgramTool);
        registry.register(ManageTodosTool);
        registry.register(SystemControlTool);
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
        let diff = diff_text(&old, &content);
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!("写入文件 {}", path.display()),
                diff: Some(diff),
            })
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
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!("编辑文件 {}", path.display()),
                diff: Some(diff_text(&old, &new)),
            })
            .await?;
        tokio::fs::write(&path, new)
            .await
            .with_context(|| format!("写入文件失败: {}", path.display()))?;
        Ok(ToolOutput::ok(format!("已编辑 {}", path.display())))
    }
}

struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "在当前工作目录执行 shell 命令，执行前请求确认"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"command":{"type":"string"},"timeout":{"type":"integer","description":"超时时间，单位秒，默认 30"}},"required":["command"]})
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let command = string_arg(&args, "command")?;
        let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
        context
            .confirm(PermissionRequest {
                tool_name: self.name().to_string(),
                summary: format!("执行命令: {command}"),
                diff: None,
            })
            .await?;
        let started = Instant::now();
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
        Ok(ToolOutput {
            content: rendered,
            is_error: !output.status.success(),
        })
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
        let absolute_pattern = context.cwd.join(pattern).to_string_lossy().to_string();
        let mut matches = Vec::new();
        for entry in glob(&absolute_pattern)? {
            let path = entry?;
            if path.is_file() {
                matches.push(
                    path.strip_prefix(&context.cwd)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                );
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
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
        {
            let path = entry.path();
            if path.components().any(|component| {
                component.as_os_str() == "target" || component.as_os_str() == ".git"
            }) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
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

struct CallModelTool;

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
                "model":{"type":"string","description":"要调用的模型名称，不能是主模型 Claude-Opus-4.6 时用于委托其他模型"},
                "prompt":{"type":"string","description":"发送给目标模型的任务内容"},
                "system":{"type":"string","description":"可选 system 指令"},
                "max_tokens":{"type":"integer","description":"最大输出 token，默认 2048"}
            },
            "required":["model","prompt"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let model = string_arg(&args, "model")?;
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
                "timeout":{"type":"integer","description":"超时时间，单位秒，默认 30"}
            },
            "required":["language","code"]
        })
    }
    async fn execute(&self, args: Value, context: &mut ToolContext) -> Result<ToolOutput> {
        let language = string_arg(&args, "language")?.to_lowercase();
        let code = string_arg(&args, "code")?;
        let timeout_secs = optional_u64_arg(&args, "timeout", 30).clamp(1, 600);
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
                summary: format!("执行 {} 代码片段", language),
                diff: None,
            })
            .await?;
        run_command(command, &context.cwd, timeout_secs).await
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

async fn run_command(command: Vec<String>, cwd: &Path, timeout_secs: u64) -> Result<ToolOutput> {
    anyhow::ensure!(!command.is_empty(), "命令不能为空");
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn context(dir: &Path) -> ToolContext {
        ToolContext::new(
            dir.to_path_buf(),
            "test-key".to_string(),
            true,
            Arc::new(AlwaysAllowPrompter),
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
}
