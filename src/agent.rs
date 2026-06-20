use crate::config::load_project_memory;
use crate::llm::{ChatRequest, LlmClient};
use crate::tools::{PermissionPrompter, PermissionRequest, ToolContext, ToolRegistry};
use crate::tui;
use crate::types::{AgentMode, AgentOptions, ContentBlock, Message, Role, StreamEvent};
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const TOKEN_THRESHOLD: usize = 24_000;

pub struct Agent<C: LlmClient> {
    client: C,
    tools: ToolRegistry,
    history: Vec<Message>,
    system_prompt: String,
    context: ToolContext,
    options: AgentOptions,
}

impl<C: LlmClient> Agent<C> {
    pub fn new(
        client: C,
        cwd: PathBuf,
        api_key: String,
        options: AgentOptions,
        prompter: Arc<dyn PermissionPrompter>,
    ) -> Result<Self> {
        let system_prompt = build_system_prompt(options.mode)?;
        Ok(Self {
            client,
            tools: ToolRegistry::builtin(),
            history: Vec::new(),
            system_prompt,
            context: ToolContext::new(cwd, api_key, options.dangerously_skip_permissions, prompter),
            options,
        })
    }

    pub fn mode(&self) -> AgentMode {
        self.options.mode
    }

    pub fn set_mode(&mut self, mode: AgentMode) -> Result<()> {
        self.options.mode = mode;
        self.system_prompt = build_system_prompt(mode)?;
        Ok(())
    }

    fn refresh_system_prompt(&mut self) -> Result<()> {
        self.system_prompt = build_system_prompt(self.options.mode)?;
        Ok(())
    }

    pub fn clear(&mut self) -> Result<()> {
        self.history.clear();
        self.refresh_system_prompt()?;
        Ok(())
    }

    pub fn estimated_tokens(&self) -> usize {
        estimate_tokens(&self.system_prompt)
            + self
                .history
                .iter()
                .map(|message| estimate_tokens(&message.text()))
                .sum::<usize>()
    }

    pub async fn run_turn(&mut self, input: String) -> Result<String> {
        self.history.push(Message::user(input));
        self.compress_if_needed();
        let started = Instant::now();
        let mut final_text = String::new();

        loop {
            let request = ChatRequest {
                model: self.options.model.clone(),
                max_tokens: self.options.max_tokens,
                system: Some(self.system_prompt.clone()),
                messages: self.history.clone(),
                tools: self.tools.definitions(),
            };

            let mut stream = self.client.stream_messages(request).await?;
            let mut assistant_blocks = Vec::new();
            let mut text_accumulator = String::new();
            let mut tool_calls = Vec::new();

            while let Some(event) = stream.recv().await {
                match event? {
                    StreamEvent::TextDelta(delta) => {
                        tui::print_agent_delta(&delta)?;
                        text_accumulator.push_str(&delta);
                        final_text.push_str(&delta);
                    }
                    StreamEvent::ToolUse(call) => {
                        if !text_accumulator.is_empty() {
                            assistant_blocks
                                .push(ContentBlock::text(std::mem::take(&mut text_accumulator)));
                        }
                        assistant_blocks.push(ContentBlock::ToolUse {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            input: call.input.clone(),
                        });
                        tool_calls.push(call);
                    }
                    StreamEvent::MessageStop => break,
                }
            }

            if !text_accumulator.is_empty() {
                assistant_blocks.push(ContentBlock::text(text_accumulator));
            }
            if !assistant_blocks.is_empty() {
                self.history.push(Message {
                    role: Role::Assistant,
                    content: assistant_blocks,
                });
            }

            if tool_calls.is_empty() {
                tui::print_agent_done(started.elapsed().as_secs_f32(), self.estimated_tokens());
                return Ok(final_text);
            }

            for call in tool_calls {
                let summary = call.input.to_string();
                let tool_started = Instant::now();
                tui::print_tool_start(&call.name, &summary);
                let output = self
                    .tools
                    .execute(&call.name, call.input, &mut self.context)
                    .await;
                tui::print_tool_done(!output.is_error, tool_started.elapsed().as_secs_f32());
                self.history.push(Message::tool_result(
                    call.id,
                    output.content,
                    output.is_error,
                ));
            }
            self.compress_if_needed();
        }
    }

    fn compress_if_needed(&mut self) {
        if self.estimated_tokens() <= TOKEN_THRESHOLD || self.history.len() < 8 {
            return;
        }
        let keep_from = self.history.len().saturating_sub(6);
        let old = self.history.drain(..keep_from).collect::<Vec<_>>();
        let summary = old
            .iter()
            .map(|message| format!("{:?}: {}", message.role, message.text()))
            .collect::<Vec<_>>()
            .join("\n");
        self.history.insert(
            0,
            Message::system(format!(
                "早期对话摘要（字符级压缩）:\n{}",
                truncate_chars(&summary, 4000)
            )),
        );
    }
}

fn build_system_prompt(mode: AgentMode) -> Result<String> {
    let mut system_prompt = base_system_prompt();
    system_prompt.push_str("\n\n当前工作模式: ");
    system_prompt.push_str(mode.as_str());
    system_prompt.push_str("\n");
    system_prompt.push_str(mode_prompt(mode));
    if let Some(memory) = load_project_memory()? {
        system_prompt.push_str("\n\n项目记忆 (.yunzhi/memory.md):\n");
        system_prompt.push_str(&memory);
    }
    Ok(system_prompt)
}

fn base_system_prompt() -> String {
    "你是云智 One，一个在终端内协助软件开发的智能体。主模型是 Claude-Opus-4.6。你可以调用工具读取、搜索、编辑文件、执行命令、执行代码片段、运行程序、管理和跟踪代办任务、执行受控系统操作，也可以在需要低成本推理、专门任务或交叉检查时调用 call_model 委托其他模型。修改文件、执行代码、运行程序、执行命令或危险系统操作前会请求用户确认。优先给出简洁、准确、可执行的回答。".to_string()
}

fn mode_prompt(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Chat => "chat 模式：以解释、问答和轻量建议为主。除非用户明确要求修改或运行，否则优先不调用会改变环境的工具。",
        AgentMode::PlanAct => "plan&act 模式：先给出短计划，确认目标和风险后分步执行；执行过程中持续更新计划并验证结果。",
        AgentMode::Entanglement => "entanglement 模式：把用户、代码、工具和其他模型视为协同上下文；主动交叉检查关键判断，在复杂任务中用 call_model 分离子问题。",
        AgentMode::Agent => "agent 模式：默认自主完成软件开发任务；在需求清楚时直接读取、编辑、测试和总结，遇到高风险操作再请求确认。",
        AgentMode::Team => "team 模式：模拟小团队协作，把任务拆成架构、实现、测试、审查等角色视角；必要时委派其他模型处理子任务并汇总决策。",
        AgentMode::Analyze => "analyze 模式：以只读分析、定位问题、风险评估和方案比较为主；除非用户明确授权，避免修改文件或执行破坏性操作。",
    }
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

pub struct DenyPrompter;

#[async_trait]
impl PermissionPrompter for DenyPrompter {
    async fn confirm(
        &self,
        _request: PermissionRequest,
    ) -> Result<crate::tools::PermissionDecision> {
        Ok(crate::tools::PermissionDecision::Deny)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_rounds_up() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
