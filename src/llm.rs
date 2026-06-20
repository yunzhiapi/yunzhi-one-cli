use crate::types::{ContentBlock, Message, StreamEvent, ToolCall, ToolDefinition};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_BASE_URL: &str = "https://yunzhiapi.cn/v1";

#[async_trait]
pub trait LlmClient: Send + Sync {
	async fn stream_messages(&self, request: ChatRequest) -> Result<mpsc::Receiver<Result<StreamEvent>>>;
}

#[derive(Debug, Clone)]
pub struct AnthropicLikeClient {
	http: reqwest::Client,
	base_url: String,
	api_key: String,
}

impl AnthropicLikeClient {
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
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
	pub model: String,
	pub max_tokens: u32,
	pub system: Option<String>,
	pub messages: Vec<Message>,
	pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Serialize)]
struct ApiRequest<'a> {
	model: &'a str,
	max_tokens: u32,
	stream: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	system: Option<&'a str>,
	messages: Vec<ApiMessage<'a>>,
	#[serde(skip_serializing_if = "is_empty_tools")]
	tools: &'a [ToolDefinition],
}

fn is_empty_tools(tools: &[ToolDefinition]) -> bool {
	tools.is_empty()
}

#[derive(Debug, Serialize)]
struct ApiMessage<'a> {
	role: &'a str,
	content: &'a [ContentBlock],
}

#[async_trait]
impl LlmClient for AnthropicLikeClient {
	async fn stream_messages(&self, request: ChatRequest) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
		let response = self.send_with_retry(&request).await?;
		let mut bytes_stream = response.bytes_stream();
		let (tx, rx) = mpsc::channel(128);

		tokio::spawn(async move {
			let mut buffer = String::new();
			let mut pending_tool: Option<PendingTool> = None;

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
					match parse_sse_event(&raw_event, &mut pending_tool) {
						Ok(Some(event)) => {
							let stop = matches!(event, StreamEvent::MessageStop);
							if tx.send(Ok(event)).await.is_err() {
								return;
							}
							if stop {
								return;
							}
						}
						Ok(None) => {}
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

impl AnthropicLikeClient {
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
		let messages = request
			.messages
			.iter()
			.filter(|message| !matches!(message.role, crate::types::Role::System))
			.map(|message| ApiMessage {
				role: match message.role {
					crate::types::Role::User | crate::types::Role::Tool => "user",
					crate::types::Role::Assistant => "assistant",
					crate::types::Role::System => "user",
				},
				content: &message.content,
			})
			.collect::<Vec<_>>();

		let body = ApiRequest {
			model: &request.model,
			max_tokens: request.max_tokens,
			stream: true,
			system: request.system.as_deref(),
			messages,
			tools: &request.tools,
		};

		let response = self
			.http
			.post(format!("{}/messages", self.base_url))
			.header("x-api-key", &self.api_key)
			.bearer_auth(&self.api_key)
			.header("anthropic-version", "2023-06-01")
			.json(&body)
			.send()
			.await
			.context("请求云智 API 失败")?;

		if response.status() == StatusCode::UNAUTHORIZED || response.status() == StatusCode::FORBIDDEN {
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

#[derive(Debug, Clone)]
struct PendingTool {
	id: String,
	name: String,
	input_json: String,
}

fn parse_sse_event(raw: &str, pending_tool: &mut Option<PendingTool>) -> Result<Option<StreamEvent>> {
	let data = raw
		.lines()
		.filter_map(|line| line.strip_prefix("data:"))
		.map(str::trim_start)
		.collect::<Vec<_>>()
		.join("\n");

	if data.is_empty() || data == "[DONE]" {
		return Ok(Some(StreamEvent::MessageStop));
	}

	let value: Value = serde_json::from_str(&data).with_context(|| format!("解析 SSE 数据失败: {data}"))?;
	let event_type = value.get("type").and_then(Value::as_str).unwrap_or_default();

	match event_type {
		"content_block_start" => {
			if value.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use") {
				*pending_tool = Some(PendingTool {
					id: value.pointer("/content_block/id").and_then(Value::as_str).unwrap_or_default().to_string(),
					name: value.pointer("/content_block/name").and_then(Value::as_str).unwrap_or_default().to_string(),
					input_json: String::new(),
				});
			}
			Ok(None)
		}
		"content_block_delta" => {
			if let Some(text) = value.pointer("/delta/text").and_then(Value::as_str) {
				return Ok(Some(StreamEvent::TextDelta(text.to_string())));
			}
			if let Some(partial_json) = value.pointer("/delta/partial_json").and_then(Value::as_str) {
				if let Some(tool) = pending_tool.as_mut() {
					tool.input_json.push_str(partial_json);
				}
			}
			Ok(None)
		}
		"content_block_stop" => {
			if let Some(tool) = pending_tool.take() {
				let input = if tool.input_json.trim().is_empty() {
					Value::Object(Default::default())
				} else {
					serde_json::from_str(&tool.input_json).context("解析工具调用参数失败")?
				};
				return Ok(Some(StreamEvent::ToolUse(ToolCall { id: tool.id, name: tool.name, input })));
			}
			Ok(None)
		}
		"message_stop" => Ok(Some(StreamEvent::MessageStop)),
		"error" => anyhow::bail!("云智 API 流式错误: {}", value),
		_ => Ok(None),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_text_delta() {
		let mut pending = None;
		let raw = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n";
		let event = parse_sse_event(raw, &mut pending).unwrap().unwrap();
		assert!(matches!(event, StreamEvent::TextDelta(text) if text == "hi"));
	}

	#[test]
	fn parses_tool_use() {
		let mut pending = None;
		parse_sse_event("data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"read_file\"}}\n", &mut pending).unwrap();
		parse_sse_event("data: {\"type\":\"content_block_delta\",\"delta\":{\"partial_json\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}\n", &mut pending).unwrap();
		let event = parse_sse_event("data: {\"type\":\"content_block_stop\"}\n", &mut pending).unwrap().unwrap();
		assert!(matches!(event, StreamEvent::ToolUse(call) if call.name == "read_file" && call.input["path"] == "README.md"));
	}
}
