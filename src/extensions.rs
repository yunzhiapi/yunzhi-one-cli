use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;
use walkdir::WalkDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub id: String,
    pub path: PathBuf,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct McpConfigFile {
    #[serde(default)]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    servers: BTreeMap<String, McpServerConfig>,
    #[serde(
        default,
        rename = "mcpServers",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    mcp_servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionScope {
    Project,
    User,
}

impl ExtensionScope {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "project" | "local" | "workspace" => Ok(Self::Project),
            "user" | "global" => Ok(Self::User),
            other => anyhow::bail!("未知扩展作用域: {other}，可选 project 或 user"),
        }
    }
}

pub fn skills_index(cwd: &Path) -> Result<Vec<SkillInfo>> {
    let mut skills = Vec::new();
    for root in skill_roots(cwd) {
        collect_skills_from_root(&root, &mut skills)?;
    }
    skills.sort_by(|a, b| a.id.cmp(&b.id));
    skills.dedup_by(|a, b| a.id == b.id);
    Ok(skills)
}

pub fn render_skills_index(cwd: &Path) -> Result<Option<String>> {
    let skills = skills_index(cwd)?;
    if skills.is_empty() {
        return Ok(None);
    }
    let rendered = skills
        .into_iter()
        .map(|skill| format!("- {}: {}", skill.id, skill.description))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Some(rendered))
}

pub fn read_skill(cwd: &Path, id_or_path: &str) -> Result<(SkillInfo, String)> {
    let requested = id_or_path.trim();
    anyhow::ensure!(!requested.is_empty(), "skill 不能为空");
    for skill in skills_index(cwd)? {
        if skill.id == requested || skill.path.to_string_lossy() == requested {
            let content = std::fs::read_to_string(&skill.path)
                .with_context(|| format!("读取 Skill 失败: {}", skill.path.display()))?;
            return Ok((skill, content));
        }
    }
    anyhow::bail!("未找到 Skill: {requested}")
}

pub fn add_skill(
    cwd: &Path,
    scope: ExtensionScope,
    id: &str,
    description: &str,
    body: &str,
    overwrite: bool,
) -> Result<PathBuf> {
    let id = normalize_extension_id(id, "skill")?;
    let root = skill_root_for_scope(cwd, scope)?;
    let path = root.join(&id).join("SKILL.md");
    if path.exists() && !overwrite {
        anyhow::bail!(
            "Skill 已存在: {}，如需覆盖请设置 overwrite=true",
            path.display()
        );
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Skill 路径无父目录: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("创建 Skill 目录失败: {}", parent.display()))?;
    let content = render_skill_content(&id, description, body);
    std::fs::write(&path, content)
        .with_context(|| format!("写入 Skill 失败: {}", path.display()))?;
    Ok(path)
}

pub fn load_mcp_servers(cwd: &Path) -> Result<BTreeMap<String, McpServerConfig>> {
    let mut servers = BTreeMap::new();
    for path in mcp_config_paths(cwd) {
        if !path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("读取 MCP 配置失败: {}", path.display()))?;
        let config = serde_json::from_str::<McpConfigFile>(&raw)
            .with_context(|| format!("解析 MCP 配置失败: {}", path.display()))?;
        servers.extend(config.servers);
        servers.extend(config.mcp_servers);
    }
    Ok(servers)
}

pub fn add_mcp_server(
    cwd: &Path,
    scope: ExtensionScope,
    name: &str,
    server: McpServerConfig,
    overwrite: bool,
) -> Result<PathBuf> {
    let name = normalize_extension_id(name, "MCP server")?;
    anyhow::ensure!(!server.command.trim().is_empty(), "MCP command 不能为空");
    let path = mcp_config_path_for_scope(cwd, scope)?;
    let mut config = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("读取 MCP 配置失败: {}", path.display()))?;
        serde_json::from_str::<McpConfigFile>(&raw)
            .with_context(|| format!("解析 MCP 配置失败: {}", path.display()))?
    } else {
        McpConfigFile::default()
    };
    if config.mcp_servers.contains_key(&name) && !overwrite {
        anyhow::bail!("MCP server 已存在: {name}，如需覆盖请设置 overwrite=true");
    }
    config.servers.remove(&name);
    config.mcp_servers.insert(name, server);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 MCP 配置目录失败: {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(&config).context("序列化 MCP 配置失败")?;
    std::fs::write(&path, format!("{raw}\n"))
        .with_context(|| format!("写入 MCP 配置失败: {}", path.display()))?;
    Ok(path)
}

pub async fn call_mcp_tool(
    cwd: &Path,
    server_name: &str,
    tool_name: &str,
    arguments: Value,
    timeout_secs: u64,
) -> Result<Value> {
    call_mcp_method(
        cwd,
        server_name,
        "tools/call",
        json!({"name": tool_name, "arguments": arguments}),
        timeout_secs,
    )
    .await
}

pub async fn list_mcp_resources(cwd: &Path, server_name: &str, timeout_secs: u64) -> Result<Value> {
    call_mcp_method(cwd, server_name, "resources/list", json!({}), timeout_secs).await
}

pub async fn read_mcp_resource(
    cwd: &Path,
    server_name: &str,
    uri: &str,
    timeout_secs: u64,
) -> Result<Value> {
    call_mcp_method(
        cwd,
        server_name,
        "resources/read",
        json!({"uri": uri}),
        timeout_secs,
    )
    .await
}

pub async fn list_mcp_prompts(cwd: &Path, server_name: &str, timeout_secs: u64) -> Result<Value> {
    call_mcp_method(cwd, server_name, "prompts/list", json!({}), timeout_secs).await
}

pub async fn get_mcp_prompt(
    cwd: &Path,
    server_name: &str,
    name: &str,
    arguments: Value,
    timeout_secs: u64,
) -> Result<Value> {
    call_mcp_method(
        cwd,
        server_name,
        "prompts/get",
        json!({"name": name, "arguments": arguments}),
        timeout_secs,
    )
    .await
}

pub async fn call_mcp_method(
    cwd: &Path,
    server_name: &str,
    method: &str,
    params: Value,
    timeout_secs: u64,
) -> Result<Value> {
    let servers = load_mcp_servers(cwd)?;
    let server = servers
        .get(server_name)
        .ok_or_else(|| anyhow!("未找到 MCP server: {server_name}"))?;

    let mut command = Command::new(&server.command);
    command
        .args(&server.args)
        .envs(&server.env)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("启动 MCP server 失败: {server_name}"))?;
    let mut stdin = child.stdin.take().context("MCP server stdin 不可用")?;
    let mut stdout = child.stdout.take().context("MCP server stdout 不可用")?;

    let run =
        async {
            send_jsonrpc(&mut stdin, json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "yunzhi-one-cli", "version": env!("CARGO_PKG_VERSION")}
            }
        }))
        .await?;
            read_response_with_id(&mut stdout, 1).await?;
            send_jsonrpc(
                &mut stdin,
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized",
                    "params": {}
                }),
            )
            .await?;
            send_jsonrpc(
                &mut stdin,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": method,
                    "params": params
                }),
            )
            .await?;
            read_response_with_id(&mut stdout, 2).await
        };

    let result = timeout(Duration::from_secs(timeout_secs.clamp(1, 600)), run)
        .await
        .map_err(|_| anyhow!("MCP 调用超时: {server_name}/{method}"))?;
    let _ = child.kill().await;
    result
}

fn skill_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.join(".yunzhi").join("skills")];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".yunzhi").join("skills"));
    }
    roots
}

fn skill_root_for_scope(cwd: &Path, scope: ExtensionScope) -> Result<PathBuf> {
    match scope {
        ExtensionScope::Project => Ok(cwd.join(".yunzhi").join("skills")),
        ExtensionScope::User => dirs::home_dir()
            .map(|home| home.join(".yunzhi").join("skills"))
            .ok_or_else(|| anyhow!("无法确定用户主目录")),
    }
}

fn collect_skills_from_root(root: &Path, skills: &mut Vec<SkillInfo>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(root)
        .min_depth(1)
        .max_depth(3)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !matches!(file_name, "SKILL.md" | "skill.md")
            && path.extension().and_then(|ext| ext.to_str()) != Some("md")
        {
            continue;
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("读取 Skill 失败: {}", path.display()))?;
        let id = skill_id(root, path);
        skills.push(SkillInfo {
            id,
            path: path.to_path_buf(),
            description: skill_description(&raw),
        });
    }
    Ok(())
}

fn skill_id(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    if matches!(
        relative.file_name().and_then(|name| name.to_str()),
        Some("SKILL.md" | "skill.md")
    ) {
        return relative
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(relative)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
    }
    relative
        .with_extension("")
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn skill_description(raw: &str) -> String {
    if let Some(frontmatter) = raw
        .strip_prefix("---")
        .and_then(|rest| rest.split_once("---"))
    {
        for line in frontmatter.0.lines() {
            if let Some(description) = line.trim().strip_prefix("description:") {
                return description.trim().trim_matches('"').to_string();
            }
        }
    }
    raw.lines()
        .find_map(|line| line.trim().strip_prefix("# ").map(str::trim))
        .filter(|heading| !heading.is_empty())
        .unwrap_or("无描述")
        .to_string()
}

fn mcp_config_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut paths = vec![cwd.join(".yunzhi").join("mcp.json")];
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".yunzhi").join("mcp.json"));
    }
    paths
}

fn mcp_config_path_for_scope(cwd: &Path, scope: ExtensionScope) -> Result<PathBuf> {
    match scope {
        ExtensionScope::Project => Ok(cwd.join(".yunzhi").join("mcp.json")),
        ExtensionScope::User => dirs::home_dir()
            .map(|home| home.join(".yunzhi").join("mcp.json"))
            .ok_or_else(|| anyhow!("无法确定用户主目录")),
    }
}

fn normalize_extension_id(id: &str, kind: &str) -> Result<String> {
    let id = id.trim().trim_matches('/').to_string();
    anyhow::ensure!(!id.is_empty(), "{kind} 名称不能为空");
    anyhow::ensure!(
        !id.contains("..")
            && !id.starts_with('.')
            && id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '/')),
        "{kind} 名称只能包含字母、数字、-、_ 和 /，且不能包含 .. 或隐藏路径"
    );
    Ok(id)
}

fn render_skill_content(id: &str, description: &str, body: &str) -> String {
    let description = description.trim();
    let title = id
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(id);
    let body = body.trim();
    format!(
        "---\ndescription: {}\n---\n# {}\n\n{}\n",
        yaml_quote(description),
        title,
        body
    )
}

fn yaml_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

async fn send_jsonrpc(stdin: &mut tokio::process::ChildStdin, value: Value) -> Result<()> {
    let body = serde_json::to_vec(&value)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin.write_all(header.as_bytes()).await?;
    stdin.write_all(&body).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_response_with_id(stdout: &mut tokio::process::ChildStdout, id: u64) -> Result<Value> {
    loop {
        let message = read_framed_json(stdout).await?;
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = message.get("error") {
            anyhow::bail!("MCP server 返回错误: {error}");
        }
        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
    }
}

async fn read_framed_json(stdout: &mut tokio::process::ChildStdout) -> Result<Value> {
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        stdout.read_exact(&mut byte).await?;
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        anyhow::ensure!(header.len() <= 8192, "MCP 响应头过长");
    }
    let header = String::from_utf8(header).context("MCP 响应头不是 UTF-8")?;
    let content_length = header
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .ok_or_else(|| anyhow!("MCP 响应缺少 Content-Length"))?;
    let mut body = vec![0_u8; content_length];
    stdout.read_exact(&mut body).await?;
    serde_json::from_slice(&body).context("解析 MCP JSON-RPC 响应失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discovers_project_skills() {
        let dir = tempdir().unwrap();
        let skill_dir = dir.path().join(".yunzhi/skills/review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: Code review helper\n---\n# Review\n",
        )
        .unwrap();

        let skills = skills_index(dir.path()).unwrap();
        assert!(skills
            .iter()
            .any(|skill| { skill.id == "review" && skill.description == "Code review helper" }));
    }

    #[test]
    fn loads_mcp_servers_from_project_config() {
        let dir = tempdir().unwrap();
        let config_dir = dir.path().join(".yunzhi");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"node","args":["server.js"]}}}"#,
        )
        .unwrap();

        let servers = load_mcp_servers(dir.path()).unwrap();
        assert_eq!(servers["fs"].command, "node");
        assert_eq!(servers["fs"].args, vec!["server.js"]);
    }

    #[test]
    fn adds_project_skill() {
        let dir = tempdir().unwrap();

        let path = add_skill(
            dir.path(),
            ExtensionScope::Project,
            "code/review",
            "Review helper",
            "Use care.",
            false,
        )
        .unwrap();

        assert!(path.ends_with(".yunzhi/skills/code/review/SKILL.md"));
        let skills = skills_index(dir.path()).unwrap();
        assert!(skills.iter().any(|skill| skill.id == "code/review"));
        let (_, content) = read_skill(dir.path(), "code/review").unwrap();
        assert!(content.contains("Use care."));
    }

    #[test]
    fn adds_project_mcp_server() {
        let dir = tempdir().unwrap();

        let path = add_mcp_server(
            dir.path(),
            ExtensionScope::Project,
            "demo",
            McpServerConfig {
                command: "node".to_string(),
                args: vec!["server.js".to_string()],
                env: BTreeMap::from([("TOKEN".to_string(), "abc".to_string())]),
            },
            false,
        )
        .unwrap();

        assert!(path.ends_with(".yunzhi/mcp.json"));
        let servers = load_mcp_servers(dir.path()).unwrap();
        assert_eq!(servers["demo"].command, "node");
        assert_eq!(servers["demo"].args, vec!["server.js"]);
        assert_eq!(servers["demo"].env["TOKEN"], "abc");
    }
}
