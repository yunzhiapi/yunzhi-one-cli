use crate::types::{ContentBlock, Message, StreamEvent, ToolCall, ToolDefinition};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_BASE_URL: &str = "https://yunzhiapi.cn/v1";
pub const DEFAULT_MODEL: &str = "Claude-Opus-4.6";

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn stream_messages(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>>;
}

#[derive(Debug, Clone)]
pub struct ChatCompletionsClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl ChatCompletionsClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, api_key)
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    pub async fn complete_once(
        &self,
        model: &str,
        system: Option<&str>,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<String> {
        let mut messages = Vec::new();
        if let Some(system) = system {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(system.to_string()),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: Some(prompt.to_string()),
            tool_calls: None,
            tool_call_id: None,
        });
        let body = ChatCompletionsRequest {
            model,
            max_tokens,
            stream: false,
            messages,
            tools: Vec::new(),
            tool_choice: None,
        };
        let response = self.send_json(body).await?;
        let value: Value = response.json().await.context("解析模型响应失败")?;
        Ok(value
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}

pub type AnthropicLikeClient = ChatCompletionsClient;

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: ToolChoice,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    Required,
    Function(String),
}

impl ToolChoice {
    fn to_value(&self) -> Value {
        match self {
            Self::Auto => json!("auto"),
            Self::Required => json!("required"),
            Self::Function(name) => json!({
                "type": "function",
                "function": { "name": name }
            }),
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionsRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ChatTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatTool {
    #[serde(rename = "type")]
    tool_type: &'static str,
    function: ChatToolFunction,
}

#[derive(Debug, Serialize)]
struct ChatToolFunction {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Serialize)]
struct ChatToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: &'static str,
    function: ChatToolCallFunction,
}

#[derive(Debug, Serialize)]
struct ChatToolCallFunction {
    name: String,
    arguments: String,
}

#[async_trait]
impl LlmClient for ChatCompletionsClient {
    async fn stream_messages(
        &self,
        request: ChatRequest,
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        let response = self.send_with_retry(&request).await?;
        let mut bytes_stream = response.bytes_stream();
        let (tx, rx) = mpsc::channel(128);

        tokio::spawn(async move {
            let mut buffer = String::new();
            let mut pending_tools = BTreeMap::new();

            while let Some(chunk) = bytes_stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        let _ = tx.send(Err(error.into())).await;
                        return;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(index) = buffer.find("\n\n") {
                    let raw_event = buffer[..index].to_string();
                    buffer = buffer[index + 2..].to_string();
                    if raw_event.trim().is_empty() {
                        continue;
                    }
                    match parse_sse_event(&raw_event, &mut pending_tools) {
                        Ok(events) => {
                            for event in events {
                                let stop = matches!(event, StreamEvent::MessageStop);
                                if tx.send(Ok(event)).await.is_err() {
                                    return;
                                }
                                if stop {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    }
                }
            }

            let _ = tx.send(Ok(StreamEvent::MessageStop)).await;
        });

        Ok(rx)
    }
}

impl ChatCompletionsClient {
    async fn send_with_retry(&self, request: &ChatRequest) -> Result<reqwest::Response> {
        let mut last_error = None;
        for attempt in 0..3 {
            match self.send_once(request).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    last_error = Some(error);
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(300 * 2_u64.pow(attempt))).await;
                    }
                }
            }
        }
        Err(last_error.expect("retry loop records last error"))
    }

    async fn send_once(&self, request: &ChatRequest) -> Result<reqwest::Response> {
        let body = ChatCompletionsRequest {
            model: &request.model,
            max_tokens: request.max_tokens,
            stream: true,
            messages: to_chat_messages(request.system.as_deref(), &request.messages),
            tools: request.tools.iter().map(to_chat_tool).collect(),
            tool_choice: (!request.tools.is_empty()).then(|| request.tool_choice.to_value()),
        };
        self.send_json(body).await
    }

    async fn send_json(&self, body: ChatCompletionsRequest<'_>) -> Result<reqwest::Response> {
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .header("x-api-key", &self.api_key)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("请求云智 API 失败")?;

        if response.status() == StatusCode::UNAUTHORIZED
            || response.status() == StatusCode::FORBIDDEN
        {
            anyhow::bail!("API Key 无效或权限不足，请运行 yunzhi config set-key <key> 重新配置");
        }
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("云智 API 返回错误 {}: {}", status, text);
        }

        Ok(response)
    }
}

fn to_chat_tool(definition: &ToolDefinition) -> ChatTool {
    ChatTool {
        tool_type: "function",
        function: ChatToolFunction {
            name: definition.name.clone(),
            description: definition.description.clone(),
            parameters: definition.input_schema.clone(),
        },
    }
}

fn to_chat_messages(system: Option<&str>, messages: &[Message]) -> Vec<ChatMessage> {
    let mut output = Vec::new();
    if let Some(system) = system {
        output.push(ChatMessage {
            role: "system".to_string(),
            content: Some(system.to_string()),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    for message in messages {
        match message.role {
            crate::types::Role::System => output.push(ChatMessage {
                role: "system".to_string(),
                content: Some(message.text()),
                tool_calls: None,
                tool_call_id: None,
            }),
            crate::types::Role::User => append_user_or_tool_messages(&mut output, message),
            crate::types::Role::Tool => append_user_or_tool_messages(&mut output, message),
            crate::types::Role::Assistant => output.push(assistant_message(message)),
        }
    }
    output
}

fn append_user_or_tool_messages(output: &mut Vec<ChatMessage>, message: &Message) {
    let mut text_parts = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => text_parts.push(text.clone()),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => output.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(if *is_error {
                    format!("ERROR: {content}")
                } else {
                    content.clone()
                }),
                tool_calls: None,
                tool_call_id: Some(tool_use_id.clone()),
            }),
            ContentBlock::ToolUse { .. } => {}
        }
    }
    if !text_parts.is_empty() {
        output.push(ChatMessage {
            role: "user".to_string(),
            content: Some(text_parts.join("\n")),
            tool_calls: None,
            tool_call_id: None,
        });
    }
}

fn assistant_message(message: &Message) -> ChatMessage {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => text_parts.push(text.clone()),
            ContentBlock::ToolUse { id, name, input } => tool_calls.push(ChatToolCall {
                id: id.clone(),
                call_type: "function",
                function: ChatToolCallFunction {
                    name: name.clone(),
                    arguments: input.to_string(),
                },
            }),
            ContentBlock::ToolResult { .. } => {}
        }
    }
    ChatMessage {
        role: "assistant".to_string(),
        content: (!text_parts.is_empty()).then(|| text_parts.join("\n")),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        tool_call_id: None,
    }
}

#[derive(Debug, Clone, Default)]
struct PendingTool {
    id: String,
    name: String,
    arguments: String,
}

fn parse_sse_event(
    raw: &str,
    pending_tools: &mut BTreeMap<usize, PendingTool>,
) -> Result<Vec<StreamEvent>> {
    let data = raw
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");

    if data.is_empty() {
        return Ok(Vec::new());
    }
    if data == "[DONE]" {
        return Ok(finish_pending_tools(pending_tools));
    }

    let value: Value =
        serde_json::from_str(&data).with_context(|| format!("解析 SSE 数据失败: {data}"))?;
    if let Some(error) = value.get("error") {
        anyhow::bail!("云智 API 流式错误: {}", error);
    }

    let mut events = Vec::new();
    let Some(choice) = value.pointer("/choices/0") else {
        return Ok(events);
    };
    let delta = choice.get("delta").unwrap_or(&Value::Null);
    if let Some(content) = delta.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            events.push(StreamEvent::TextDelta(content.to_string()));
        }
    }
    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for tool_call in tool_calls {
            let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let pending = pending_tools.entry(index).or_default();
            if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                pending.id = id.to_string();
            }
            if let Some(name) = tool_call.pointer("/function/name").and_then(Value::as_str) {
                pending.name = name.to_string();
            }
            if let Some(arguments) = tool_call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
            {
                pending.arguments.push_str(arguments);
            }
        }
    }
    
    // 检查是否有 finish_reason
    if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
        // 无论什么 finish_reason，都先完成待处理的工具调用
        if !pending_tools.is_empty() {
            events.extend(finish_pending_tools(pending_tools));
        }
        
        // 然后根据 finish_reason 添加相应事件
        match finish_reason {
            "tool_calls" => {
                // 工具调用已在上面处理
            }
            "stop" | "end_turn" | "max_tokens" | "length" => {
                events.push(StreamEvent::MessageStop);
            }
            _ => {
                // 未知的 finish_reason，也发送 MessageStop
                events.push(StreamEvent::MessageStop);
            }
        }
    }
    
    Ok(events)
}

fn finish_pending_tools(pending_tools: &mut BTreeMap<usize, PendingTool>) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    for (_, tool) in std::mem::take(pending_tools) {
        let input = if tool.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&tool.arguments)
                .unwrap_or_else(|_| json!({ "raw_arguments": tool.arguments }))
        };
        events.push(StreamEvent::ToolUse(ToolCall {
            id: if tool.id.is_empty() {
                format!("tool_{}", events.len())
            } else {
                tool.id
            },
            name: tool.name,
            input,
        }));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chat_text_delta() {
        let mut pending = BTreeMap::new();
        let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n";
        let events = parse_sse_event(raw, &mut pending).unwrap();
        assert!(matches!(&events[0], StreamEvent::TextDelta(text) if text == "hi"));
    }

    #[test]
    fn parses_chat_tool_call() {
        let mut pending = BTreeMap::new();
        parse_sse_event("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n", &mut pending).unwrap();
        let events = parse_sse_event("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"README.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n", &mut pending).unwrap();
        assert!(
            matches!(&events[0], StreamEvent::ToolUse(call) if call.name == "read_file" && call.input["path"] == "README.md")
        );
    }

    #[test]
    fn maps_tools_to_chat_completions_shape() {
        let tool = ToolDefinition {
            name: "read_file".to_string(),
            description: "读取文件".to_string(),
            input_schema: json!({"type":"object"}),
        };
        let mapped = serde_json::to_value(to_chat_tool(&tool)).unwrap();
        assert_eq!(mapped["type"], "function");
        assert_eq!(mapped["function"]["name"], "read_file");
        assert_eq!(mapped["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn serializes_tool_choice_auto_when_tools_exist() {
        let body = ChatCompletionsRequest {
            model: "Claude-Opus-4.6",
            max_tokens: 4096,
            stream: true,
            messages: Vec::new(),
            tools: vec![ChatTool {
                tool_type: "function",
                function: ChatToolFunction {
                    name: "write_file".to_string(),
                    description: "写入文件".to_string(),
                    parameters: json!({"type":"object"}),
                },
            }],
            tool_choice: Some(ToolChoice::Auto.to_value()),
        };
        let value = serde_json::to_value(body).unwrap();
        assert_eq!(value["tool_choice"], "auto");
    }

    #[test]
    fn serializes_forced_function_tool_choice() {
        let value = ToolChoice::Function("write_file".to_string()).to_value();
        assert_eq!(value["type"], "function");
        assert_eq!(value["function"]["name"], "write_file");
    }
}
