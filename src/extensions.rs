use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
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

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct McpConfigFile {
    #[serde(default)]
    servers: BTreeMap<String, McpServerConfig>,
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, McpServerConfig>,
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

pub async fn call_mcp_tool(
    cwd: &Path,
    server_name: &str,
    tool_name: &str,
    arguments: Value,
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
                    "method": "tools/call",
                    "params": {"name": tool_name, "arguments": arguments}
                }),
            )
            .await?;
            read_response_with_id(&mut stdout, 2).await
        };

    let result = timeout(Duration::from_secs(timeout_secs.clamp(1, 600)), run)
        .await
        .map_err(|_| anyhow!("MCP 调用超时: {server_name}/{tool_name}"))?;
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
}
