use crate::tools::{AlwaysAllowPrompter, ToolContext, ToolRegistry};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn run_stdio_server(cwd: PathBuf, api_key: String) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut context = ToolContext::new(cwd, api_key, true, Arc::new(AlwaysAllowPrompter), true);
    let tools = ToolRegistry::builtin();

    loop {
        let message = match read_framed_json(&mut stdin).await {
            Ok(message) => message,
            Err(error) if is_eof(&error) => break,
            Err(error) => return Err(error),
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = handle_request(method, params, &tools, &mut context).await;
        let response = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => json!({
                "jsonrpc":"2.0",
                "id":id,
                "error":{"code":-32000,"message":error.to_string()}
            }),
        };
        write_framed_json(&mut stdout, &response).await?;
    }
    Ok(())
}

async fn handle_request(
    method: &str,
    params: Value,
    tools: &ToolRegistry,
    context: &mut ToolContext,
) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": {},
                "resources": {},
                "prompts": {}
            },
            "serverInfo": {"name":"yunzhi-one-cli", "version": env!("CARGO_PKG_VERSION")}
        })),
        "tools/list" => Ok(json!({
            "tools": tools.definitions().into_iter().map(|tool| json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema
            })).collect::<Vec<_>>()
        })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .context("tools/call 缺少 name")?;
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let output = tools.execute(name, arguments, context).await;
            Ok(json!({
                "content": [{"type":"text", "text": output.content}],
                "isError": output.is_error
            }))
        }
        "resources/list" => Ok(json!({
            "resources": [
                {
                    "uri": "yunzhi://workspace",
                    "name": "Workspace Root",
                    "description": "Current yunzhi workspace root",
                    "mimeType": "text/plain"
                },
                {
                    "uri": "yunzhi://tools",
                    "name": "Yunzhi Tools",
                    "description": "Available yunzhi tool names",
                    "mimeType": "application/json"
                }
            ]
        })),
        "resources/read" => {
            let uri = params
                .get("uri")
                .and_then(Value::as_str)
                .context("resources/read 缺少 uri")?;
            let text = match uri {
                "yunzhi://workspace" => context.cwd.display().to_string(),
                "yunzhi://tools" => serde_json::to_string_pretty(&tools.definitions())?,
                other => anyhow::bail!("未知资源 URI: {other}"),
            };
            Ok(json!({"contents": [{"uri": uri, "mimeType": "text/plain", "text": text}]}))
        }
        "prompts/list" => Ok(json!({
            "prompts": [
                {
                    "name": "code_review",
                    "description": "Review a diff or change description with yunzhi conventions",
                    "arguments": [{"name":"diff", "description":"Diff or change description", "required": true}]
                },
                {
                    "name": "test_plan",
                    "description": "Create a focused verification plan",
                    "arguments": [{"name":"scope", "description":"Feature or file scope", "required": true}]
                }
            ]
        })),
        "prompts/get" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .context("prompts/get 缺少 name")?;
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let prompt = match name {
                "code_review" => format!(
                    "请以代码审查模式评估以下变更，优先列出 bug、回归风险和缺失测试。\n\n{}",
                    arguments
                        .get("diff")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                ),
                "test_plan" => format!(
                    "请为以下范围设计简洁但覆盖关键风险的验证计划：{}",
                    arguments
                        .get("scope")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                ),
                other => anyhow::bail!("未知 prompt: {other}"),
            };
            Ok(json!({
                "description": name,
                "messages": [{"role":"user", "content":{"type":"text", "text": prompt}}]
            }))
        }
        _ => anyhow::bail!("不支持的 MCP 方法: {method}"),
    }
}

async fn read_framed_json<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Value> {
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        reader.read_exact(&mut byte).await?;
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        anyhow::ensure!(header.len() <= 8192, "MCP 请求头过长");
    }
    let header = String::from_utf8(header).context("MCP 请求头不是 UTF-8")?;
    let content_length = header
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .context("MCP 请求缺少 Content-Length")?;
    let mut body = vec![0_u8; content_length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body).context("解析 MCP 请求失败")
}

async fn write_framed_json<W: AsyncWriteExt + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

fn is_eof(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn lists_tools_resources_and_prompts() {
        let dir = tempdir().unwrap();
        let mut context = ToolContext::new(
            dir.path().to_path_buf(),
            "test-key".to_string(),
            true,
            Arc::new(AlwaysAllowPrompter),
            true,
        );
        let tools = ToolRegistry::builtin();
        let tool_list = handle_request("tools/list", json!({}), &tools, &mut context)
            .await
            .unwrap();
        assert!(tool_list["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "read_file"));
        let resources = handle_request("resources/list", json!({}), &tools, &mut context)
            .await
            .unwrap();
        assert_eq!(resources["resources"][0]["uri"], "yunzhi://workspace");
        let prompts = handle_request("prompts/list", json!({}), &tools, &mut context)
            .await
            .unwrap();
        assert_eq!(prompts["prompts"][0]["name"], "code_review");
    }
}
