use crate::config::load_project_memory_prompt;
use crate::extensions::render_skills_index;
use crate::hooks::{format_hook_runs, run_matching_hooks, HookEvent};
use crate::llm::{ChatRequest, LlmClient, ToolChoice};
use crate::observability::{
    append_tool_audit, estimate_cost_usd, estimate_request_tokens,
    estimate_tokens as estimate_text_tokens, now_unix, truncate_preview, ToolAuditRecord,
    TurnMetrics, UsageMetrics,
};
use crate::session::{
    create_checkpoint, list_sessions, load_session, new_session, rollback_checkpoint, save_session,
};
use crate::tools::{PermissionPrompter, PermissionRequest, ToolContext, ToolRegistry};
use crate::tui;
use crate::types::{AgentMode, AgentOptions, ContentBlock, Message, Role, StreamEvent};
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const TOKEN_THRESHOLD: usize = 24_000;
const PLAN_READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "glob_search",
    "grep_search",
    "code_index",
    "file_info",
    "ask_user",
    "choose_option",
    "list_models",
    "list_skills",
    "read_skill",
    "list_mcp_servers",
    "mcp_resource",
    "mcp_prompt",
];
const ANALYZE_READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "glob_search",
    "grep_search",
    "code_index",
    "file_info",
    "ask_user",
    "choose_option",
    "list_models",
    "call_model",
    "list_skills",
    "read_skill",
    "list_mcp_servers",
    "mcp_resource",
    "mcp_prompt",
];

pub struct Agent<C: LlmClient> {
    client: C,
    tools: ToolRegistry,
    history: Vec<Message>,
    system_prompt: String,
    context: ToolContext,
    options: AgentOptions,
    pending_plan: Option<String>,
    usage: UsageMetrics,
}

impl<C: LlmClient> Agent<C> {
    pub fn new(
        client: C,
        cwd: PathBuf,
        api_key: String,
        options: AgentOptions,
        prompter: Arc<dyn PermissionPrompter>,
    ) -> Result<Self> {
        let system_prompt = build_system_prompt(&options)?;
        let auto_approve = matches!(
            options.mode,
            AgentMode::Agent | AgentMode::PlanAct | AgentMode::Team | AgentMode::Entanglement
        );
        Ok(Self {
            client,
            tools: ToolRegistry::builtin(),
            history: Vec::new(),
            system_prompt,
            context: ToolContext::new(
                cwd,
                api_key,
                options.dangerously_skip_permissions,
                prompter,
                auto_approve,
            ),
            options,
            pending_plan: None,
            usage: UsageMetrics::default(),
        })
    }

    pub fn mode(&self) -> AgentMode {
        self.options.mode
    }

    pub fn model(&self) -> &str {
        &self.options.model
    }

    pub fn set_model(&mut self, model: impl Into<String>) -> Result<()> {
        let model = model.into().trim().to_string();
        anyhow::ensure!(!model.is_empty(), "模型名称不能为空");
        self.options.model = model;
        self.system_prompt = build_system_prompt(&self.options)?;
        Ok(())
    }

    pub fn set_mode(&mut self, mode: AgentMode) -> Result<()> {
        self.options.mode = mode;
        self.system_prompt = build_system_prompt(&self.options)?;
        self.pending_plan = None;
        Ok(())
    }

    fn refresh_system_prompt(&mut self) -> Result<()> {
        self.system_prompt = build_system_prompt(&self.options)?;
        Ok(())
    }

    pub fn clear(&mut self) -> Result<()> {
        self.history.clear();
        self.pending_plan = None;
        self.usage = UsageMetrics::default();
        self.refresh_system_prompt()?;
        Ok(())
    }

    pub fn save_session(&self, id: &str) -> Result<PathBuf> {
        let mut session = new_session(id.to_string(), self.history.clone());
        session.usage = self.usage;
        save_session(&self.context.cwd, &session)
    }

    pub fn resume_session(&mut self, id: &str) -> Result<()> {
        let session = load_session(&self.context.cwd, id)?;
        self.history = session.messages;
        self.usage = session.usage;
        self.pending_plan = None;
        self.refresh_system_prompt()?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<String>> {
        list_sessions(&self.context.cwd)
    }

    pub fn share_session(&self, id: &str) -> Result<PathBuf> {
        let session = load_session(&self.context.cwd, id)?;
        let path = self
            .context
            .cwd
            .join(".yunzhi")
            .join("sessions")
            .join(format!("{id}.share.json"));
        let raw = serde_json::to_string_pretty(&session)?;
        std::fs::write(&path, raw)?;
        Ok(path)
    }

    pub fn checkpoint_session(&self, id: &str, note: Option<String>) -> Result<String> {
        let mut session = match load_session(&self.context.cwd, id) {
            Ok(mut session) => {
                session.messages = self.history.clone();
                session.usage = self.usage;
                session
            }
            Err(_) => {
                let mut session = new_session(id.to_string(), self.history.clone());
                session.usage = self.usage;
                session
            }
        };
        create_checkpoint(&self.context.cwd, &mut session, note)
    }

    pub fn rollback_session(&mut self, id: &str, checkpoint_id: &str) -> Result<()> {
        let session = load_session(&self.context.cwd, id)?;
        rollback_checkpoint(&self.context.cwd, &session, checkpoint_id)?;
        self.history = session.messages;
        self.usage = session.usage;
        self.pending_plan = None;
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
        let plan_act_execute = matches!(self.options.mode, AgentMode::PlanAct)
            && is_act_approval(&input)
            && self.pending_plan.is_some();
        let plan_act_planning =
            matches!(self.options.mode, AgentMode::PlanAct) && !plan_act_execute;
        let prepared = self.prepare_turn_input(input);
        let mut next_tool_choice = if plan_act_execute {
            ToolChoice::Required
        } else {
            tool_choice_for_input(self.options.mode, &prepared)
        };
        self.history.push(Message::user(prepared));
        self.compress_if_needed();
        let started = Instant::now();
        let mut final_text = String::new();
        let mut tool_call_count = 0;
        let mut turn_metrics = TurnMetrics::default();

        loop {
            let request = ChatRequest {
                model: self.options.model.clone(),
                max_tokens: self.options.max_tokens,
                system: Some(self.system_prompt.clone()),
                messages: self.history.clone(),
                tools: if plan_act_planning {
                    self.filtered_tool_definitions(PLAN_READ_ONLY_TOOLS)
                } else if matches!(self.options.mode, AgentMode::Analyze) {
                    self.filtered_tool_definitions(ANALYZE_READ_ONLY_TOOLS)
                } else {
                    self.filtered_all_tool_definitions()
                },
                tool_choice: next_tool_choice.clone(),
            };
            turn_metrics.request_count += 1;
            turn_metrics.input_tokens +=
                estimate_request_tokens(request.system.as_deref(), &request.messages);

            let mut stream = self.client.stream_messages(request).await?;
            let mut assistant_blocks = Vec::new();
            let mut text_accumulator = String::new();
            let mut tool_calls = Vec::new();

            while let Some(event) = stream.recv().await {
                match event? {
                    StreamEvent::TextDelta(delta) => {
                        tui::print_agent_delta(&delta)?;
                        turn_metrics.output_tokens += estimate_text_tokens(&delta);
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
                if matches!(self.options.mode, AgentMode::PlanAct) && self.pending_plan.is_none() {
                    self.pending_plan = Some(final_text.trim().to_string());
                }
                if tool_call_count == 0 && looks_like_unverified_completion(&final_text) {
                    tui::print_unverified_completion_warning();
                }
                turn_metrics.elapsed_ms = started.elapsed().as_millis() as u64;
                turn_metrics.tool_call_count = tool_call_count;
                turn_metrics.estimated_cost_usd = estimate_cost_usd(
                    &self.options.model,
                    turn_metrics.input_tokens,
                    turn_metrics.output_tokens,
                );
                self.usage.add_turn(turn_metrics);
                tui::print_agent_done(
                    turn_metrics,
                    self.usage,
                    self.estimated_tokens(),
                    &self.options.model,
                );
                return Ok(final_text);
            }

            for call in tool_calls {
                tool_call_count += 1;
                let summary = call.input.to_string();
                let tool_started = Instant::now();
                tui::print_tool_start(&call.name, &summary);
                let output = match run_matching_hooks(
                    &self.context.cwd,
                    HookEvent::PreTool,
                    &call.name,
                    &call.input,
                    None,
                )
                .await
                {
                    Ok(_) => {
                        let mut output = self
                            .tools
                            .execute(&call.name, call.input.clone(), &mut self.context)
                            .await;
                        match run_matching_hooks(
                            &self.context.cwd,
                            HookEvent::PostTool,
                            &call.name,
                            &call.input,
                            Some(&output.content),
                        )
                        .await
                        {
                            Ok(hooks) if !hooks.is_empty() => {
                                output.content.push_str("\n\n");
                                output.content.push_str(&format_hook_runs(&hooks));
                                output
                            }
                            Ok(_) => output,
                            Err(error) => {
                                output.is_error = true;
                                output.content.push_str("\n\npost_tool hook failed:\n");
                                output.content.push_str(&error.to_string());
                                output
                            }
                        }
                    }
                    Err(error) => {
                        crate::types::ToolOutput::error(format!("pre_tool hook failed:\n{}", error))
                    }
                };
                tui::print_tool_result(&call.name, &output.content, output.is_error);
                tui::print_tool_done(!output.is_error, tool_started.elapsed().as_secs_f32());
                let audit_record = ToolAuditRecord {
                    timestamp_unix: now_unix(),
                    call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    input: call.input.clone(),
                    output_preview: truncate_preview(&output.content, 4000),
                    is_error: output.is_error,
                    elapsed_ms: tool_started.elapsed().as_millis() as u64,
                };
                if let Err(error) = append_tool_audit(&self.context.cwd, &audit_record) {
                    eprintln!("审计日志写入失败: {error:#}");
                }
                self.history.push(Message::tool_result(
                    call.id,
                    output.content,
                    output.is_error,
                ));
            }
            next_tool_choice = ToolChoice::Auto;
            self.compress_if_needed();
        }
    }

    fn prepare_turn_input(&mut self, input: String) -> String {
        if matches!(self.options.mode, AgentMode::PlanAct) {
            if is_act_approval(&input) {
                if let Some(plan) = self.pending_plan.take() {
                    return format!(
                        "用户已输入 act，批准执行上一份计划。请严格按计划执行，必要时读取、编辑、运行和验证。上一份计划如下:\n\n{}",
                        plan
                    );
                }
            } else {
                self.pending_plan = None;
                return format!(
                    "请先制定计划。你可以调用只读工具了解项目现状，包括读取文件、列目录、搜索、查看文件信息和读取模型列表；严禁写入/编辑/删除文件，严禁执行命令或运行程序。计划必须基于必要的只读探查，包含目标、步骤、风险和验证方式。计划结尾必须提示用户输入 act 后才开始执行。用户请求:\n{}",
                    input
                );
            }
        }
        if matches!(self.options.mode, AgentMode::Team) {
            return format!(
                "Team 模式任务。第一步必须调用 list_models 获取可用模型列表；然后根据模型名称特征和任务需求，使用 call_model 分配架构、实现、测试、审查等子智能体任务。当前阶段完成后，必须在总结中声明下一位被唤醒的智能体、输入上下文和交付物，形成一个人一个公司的协作流。用户请求:\n{}",
                input
            );
        }
        if matches!(self.options.mode, AgentMode::Entanglement) {
            return entanglement_turn_input(&input);
        }
        if matches!(self.options.mode, AgentMode::Analyze) {
            return analyze_turn_input(&input);
        }
        input
    }

    fn filtered_all_tool_definitions(&self) -> Vec<crate::types::ToolDefinition> {
        match self.options.tool_allowlist.as_deref() {
            Some(allowlist) => {
                let names = allowlist.iter().map(String::as_str).collect::<Vec<_>>();
                self.tools.definitions_for(&names)
            }
            None => self.tools.definitions(),
        }
    }

    fn filtered_tool_definitions(
        &self,
        default_names: &[&str],
    ) -> Vec<crate::types::ToolDefinition> {
        match self.options.tool_allowlist.as_deref() {
            Some(allowlist) => self
                .tools
                .definitions_for(&filter_tool_names(default_names, allowlist)),
            None => self.tools.definitions_for(default_names),
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

fn build_system_prompt(options: &AgentOptions) -> Result<String> {
    let mut system_prompt = base_system_prompt();
    system_prompt.push_str("\n\n当前主控模型: ");
    system_prompt.push_str(&options.model);
    system_prompt.push_str("\n\n当前工作模式: ");
    system_prompt.push_str(options.mode.as_str());
    system_prompt.push_str("\n");
    system_prompt.push_str(mode_prompt(options.mode));
    if let Some(profile_name) = &options.profile_name {
        system_prompt.push_str("\n\n当前 Profile: ");
        system_prompt.push_str(profile_name);
    }
    if let Some(persona) = &options.persona {
        system_prompt.push_str("\n\nProfile 人格/项目指令:\n");
        system_prompt.push_str(persona);
    }
    if let Some(tools) = &options.tool_allowlist {
        system_prompt.push_str("\n\nProfile 工具白名单: ");
        system_prompt.push_str(&tools.join(", "));
    }
    if let Some(memory) = load_project_memory_prompt()? {
        system_prompt.push_str("\n\n项目记忆 (.yunzhi/memory.md):\n");
        system_prompt.push_str(&memory);
    }
    if let Some(skills) = render_skills_index(&std::env::current_dir()?)? {
        system_prompt.push_str("\n\n可用 Skills (.yunzhi/skills 与 ~/.yunzhi/skills):\n");
        system_prompt.push_str(&skills);
        system_prompt
            .push_str("\n需要使用某个 Skill 时，先调用 read_skill 读取完整说明，再按说明执行。");
    }
    Ok(system_prompt)
}

fn filter_tool_names<'a>(default_names: &[&'a str], allowlist: &[String]) -> Vec<&'a str> {
    default_names
        .iter()
        .copied()
        .filter(|name| allowlist.iter().any(|allowed| allowed == name))
        .collect()
}

fn base_system_prompt() -> String {
    "你是云智 One，一个在终端内协助软件开发和电脑任务的智能体。你可以调用工具读取、搜索、编辑、追加、复制、移动、删除文件，创建目录，查看文件元信息，执行命令、执行代码片段、运行程序、管理 Git、运行测试循环、管理和跟踪代办任务、询问用户、让用户选择选项、执行受控系统操作，制作 PPT，调用绘图模型生成图片，写文档和表格，管理磁盘和电脑，网络搜索，调用浏览器，获取网络日志和电脑信息，连接和管理数据库，生成 UI 智能设计规格，管理长期记忆，读取、添加并使用 Skills，也可以添加 MCP server 配置、通过 MCP server 调用外部工具、读取 MCP resources、获取 MCP prompts，还可以在需要低成本推理、专门任务或交叉检查时调用 call_model 委托其他模型。\n\n关键规则：\n1. 凡是用户要求创建、修改、删除、读取文件，或要求执行命令/代码/程序，都必须立即调用对应工具完成，不要用自然语言描述你将要做什么。\n2. 禁止在没有实际调用工具并收到工具结果前，使用「我将...」「正在...」「马上...」「请稍等」「已经完成」「已经创建」「已经写入」等表述。\n3. 如果用户请求适合某个 Skill，先调用 read_skill 获取完整说明再执行；如果用户要求新增可复用能力，用 add_skill 写入项目级或用户级 Skill；如果需要外部 MCP 能力，先调用 list_mcp_servers 确认可用 server，缺少配置时用 add_mcp_server 添加，再按需用 mcp_resource 读取资源、用 mcp_prompt 获取提示模板，只有需要执行外部工具时才调用 call_mcp_tool。\n4. 如果缺少必要信息，优先用 ask_user 收集自由文本；如果需要用户在多个方案、文件、模式或配置之间决策，优先用 choose_option 发起选择；一旦信息齐全立即调用后续工具。\n5. bash、execute_code、run_program、test_loop、copy_path、move_path、delete_path、kill_process、call_mcp_tool、disk_manager、computer_manager、browser.open、database_manager.execute 等危险操作会自动请求用户确认；bash、execute_code 和 test_loop 默认在沙箱工作区副本中运行，只有明确需要修改当前工作区时才传 sandbox=false。\n6. 涉及 Git 状态、diff、生成提交信息、创建分支、提交、推送或 PR 时，优先使用 git_manager；code review 请求应先用 git_manager action=review_diff 获取 diff，再基于 diff 输出评审。\n7. 开发任务完成前优先调用 test_loop 或项目约定测试命令验证；测试失败时根据失败摘要继续读取、编辑并重跑，直到通过或明确说明阻塞原因。\n8. 优先给出简洁、准确、可执行的回答。".to_string()
}

fn looks_like_unverified_completion(text: &str) -> bool {
    let ascii = text.to_ascii_lowercase();
    let words = [
        "完成",
        "已创建",
        "已新建",
        "已写入",
        "已修改",
        "已执行",
        "已运行",
        "操作完成",
        "我将",
        "我马上",
        "正在",
        "请稍等",
        "稍等",
        "done",
        "created",
        "written",
        "modified",
        "executed",
        "i will",
        "i'll",
        "please wait",
    ];
    words
        .iter()
        .any(|word| text.contains(word) || ascii.contains(word))
}

fn tool_choice_for_input(mode: AgentMode, input: &str) -> ToolChoice {
    if matches!(mode, AgentMode::Chat | AgentMode::Analyze) {
        return ToolChoice::Auto;
    }
    if matches!(mode, AgentMode::PlanAct) && !is_act_approval(input) {
        return ToolChoice::Auto;
    }
    if matches!(mode, AgentMode::Team) {
        return ToolChoice::Function("list_models".to_string());
    }
    if mentions_file_write(input) {
        return ToolChoice::Function("write_file".to_string());
    }
    if mentions_file_read(input) {
        return ToolChoice::Function("read_file".to_string());
    }
    if mentions_command_run(input) {
        return ToolChoice::Required;
    }
    ToolChoice::Auto
}

fn mentions_file_write(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    let write_words = [
        "新建", "创建", "写入", "保存", "生成", "修改", "编辑", "create", "write", "save",
        "modify", "edit",
    ];
    let file_words = [
        "文件", "file", ".txt", ".md", ".rs", ".json", ".toml", ".yaml", ".yml",
    ];
    write_words
        .iter()
        .any(|word| input.contains(word) || lower.contains(word))
        && file_words
            .iter()
            .any(|word| input.contains(word) || lower.contains(word))
}

fn mentions_file_read(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    let read_words = [
        "读取", "查看", "打开", "看看", "read", "show", "cat", "open",
    ];
    let file_words = [
        "文件", "file", ".txt", ".md", ".rs", ".json", ".toml", ".yaml", ".yml",
    ];
    read_words
        .iter()
        .any(|word| input.contains(word) || lower.contains(word))
        && file_words
            .iter()
            .any(|word| input.contains(word) || lower.contains(word))
}

fn mentions_command_run(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    [
        "运行", "执行", "run", "execute", "cargo ", "npm ", "python", "node ",
    ]
    .iter()
    .any(|word| input.contains(word) || lower.contains(word))
}

fn is_act_approval(input: &str) -> bool {
    matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "act" | "执行" | "开始执行"
    )
}

fn entanglement_turn_input(input: &str) -> String {
    format!(
        "纠缠模式任务。请把用户目标、代码上下文、工具结果、可用 Skills/MCP、可用模型和不确定性视为同一个协同系统处理。\n\n执行协议：\n1. 先建立纠缠图：列出目标、相关文件/命令/模型/外部能力、约束、风险和未知数。\n2. 对未知数主动调用只读工具求证；如果任务需要专门视角或反证，先用 list_models 获取候选模型，再用 call_model 委托至少一个子模型做独立检查。\n3. 形成假设矩阵：主假设、反假设、证据、反证、置信度和下一步动作。\n4. 如果需要改动文件或执行命令，先说明证据链，再调用对应工具完成；不要仅凭猜测修改。\n5. 每轮总结必须包含：已确认事实、仍纠缠的问题、被唤醒的下一上下文或子模型、最终建议/交付物。\n\n用户请求:\n{}",
        input
    )
}

fn analyze_turn_input(input: &str) -> String {
    format!(
        "Analyze 模式任务。此模式只允许只读分析工具和模型交叉审查，不允许写入、编辑、删除、执行命令、运行程序或调用 MCP 工具。\n\n分析协议：\n1. 先用只读工具收集必要证据，优先读取相关文件、列目录、搜索文本和查看文件信息。\n2. 明确区分事实、推断和不确定项；不要把未验证的猜测写成结论。\n3. 对复杂、高风险或架构性判断，可以用 list_models 和 call_model 委托一个子模型做独立审查。\n4. 输出结构必须包含：问题定位、证据、风险等级、影响范围、可选方案、推荐方案和验证建议。\n5. 如果用户要求修改或执行，只给出建议和可执行计划；提醒需要切换到 agent/team/plan-act 或显式使用执行模式。\n\n用户请求:\n{}",
        input
    )
}

fn mode_prompt(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Chat => "chat 模式：以解释、问答和轻量建议为主。除非用户明确要求修改或运行，否则优先不调用会改变环境的工具。",
        AgentMode::PlanAct => "plan&act 模式：第一轮只能制定计划，可以使用只读工具读取文件、列目录、搜索和查看信息；严禁写入、编辑、删除、执行命令或运行程序。计划末尾提示用户输入 act。只有用户明确输入 act 后，才进入执行阶段，并按计划编辑、运行和验证。",
        AgentMode::Entanglement => "entanglement 模式：把用户目标、代码、工具、Skills、MCP、可用模型和不确定性视为同一个协同系统。先建立纠缠图和假设矩阵，再用只读工具求证关键事实；复杂或高风险判断必须通过 call_model 做独立反证或交叉检查。修改前先给出证据链，完成后总结已确认事实、剩余纠缠点和下一位被唤醒的上下文/子模型。",
        AgentMode::Agent => "agent 模式：默认自主完成软件开发任务；在需求清楚时必须直接调用工具读取、编辑、测试和总结，不要用对话确认替代工具操作，遇到高风险操作由工具权限确认接管。",
        AgentMode::Team => "team 模式：主模型担任 CEO/调度器。每个新任务第一步必须调用 list_models 获取可用模型列表，然后按需求用 call_model 分配子智能体任务。子智能体可处于 waiting/running/done 状态；某智能体完成后，主模型根据交付物唤醒下一位智能体，并把必要上下文传给它，形成一个人一个公司的流水线。",
        AgentMode::Analyze => "analyze 模式：代码层只暴露只读分析工具、Skill/MCP 列表和 call_model 交叉审查能力；不提供写入、编辑、删除、执行命令、运行程序或 call_mcp_tool。重点是定位问题、证据链、风险评估、影响范围、方案比较和验证建议。若用户要求修改或执行，只输出计划并提示切换到 agent/team/plan-act 等执行模式。",
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

    async fn ask_user(&self, request: crate::tools::UserQuestionRequest) -> Result<String> {
        Ok(request.default_answer.unwrap_or_default())
    }

    async fn choose_option(
        &self,
        request: crate::tools::UserChoiceRequest,
    ) -> Result<crate::tools::UserChoiceResponse> {
        let answer = request.options.first().cloned().unwrap_or_default();
        Ok(crate::tools::UserChoiceResponse {
            answer,
            index: Some(0),
            custom: false,
        })
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

    #[test]
    fn set_model_rejects_empty_name() {
        let options = AgentOptions::default();
        let client = crate::llm::ChatCompletionsClient::new("sk-test");
        let mut agent = Agent::new(
            client,
            std::env::current_dir().unwrap(),
            "sk-test".to_string(),
            options,
            Arc::new(DenyPrompter),
        )
        .unwrap();

        agent.set_model("DeepSeek-V4-pro").unwrap();
        assert_eq!(agent.model(), "DeepSeek-V4-pro");
        assert!(agent.set_model("   ").is_err());
    }

    #[test]
    fn agent_mode_forces_file_write_tool() {
        assert_eq!(
            tool_choice_for_input(AgentMode::Agent, "新建一个txt文件，里面写上test"),
            ToolChoice::Function("write_file".to_string())
        );
    }

    #[test]
    fn chat_mode_keeps_tools_auto() {
        assert_eq!(
            tool_choice_for_input(AgentMode::Chat, "新建一个txt文件，里面写上test"),
            ToolChoice::Auto
        );
    }

    #[test]
    fn team_mode_starts_with_model_discovery() {
        assert_eq!(
            tool_choice_for_input(AgentMode::Team, "实现一个功能"),
            ToolChoice::Function("list_models".to_string())
        );
    }

    #[test]
    fn entanglement_wraps_turn_with_protocol() {
        let prepared = entanglement_turn_input("分析一个复杂问题");
        assert!(prepared.contains("纠缠图"));
        assert!(prepared.contains("假设矩阵"));
        assert!(prepared.contains("call_model"));
        assert!(prepared.contains("用户请求:\n分析一个复杂问题"));
    }

    #[test]
    fn analyze_wraps_turn_with_read_only_protocol() {
        let prepared = analyze_turn_input("评审实现风险");
        assert!(prepared.contains("只允许只读分析工具"));
        assert!(prepared.contains("风险等级"));
        assert!(prepared.contains("用户请求:\n评审实现风险"));
    }

    #[test]
    fn recognizes_act_approval() {
        assert!(is_act_approval("act"));
        assert!(is_act_approval("执行"));
        assert!(!is_act_approval("先写计划"));
    }

    #[test]
    fn plan_read_only_tools_exclude_mutating_tools() {
        assert!(PLAN_READ_ONLY_TOOLS.contains(&"read_file"));
        assert!(PLAN_READ_ONLY_TOOLS.contains(&"grep_search"));
        assert!(PLAN_READ_ONLY_TOOLS.contains(&"code_index"));
        assert!(PLAN_READ_ONLY_TOOLS.contains(&"ask_user"));
        assert!(PLAN_READ_ONLY_TOOLS.contains(&"choose_option"));
        assert!(PLAN_READ_ONLY_TOOLS.contains(&"list_skills"));
        assert!(PLAN_READ_ONLY_TOOLS.contains(&"read_skill"));
        assert!(PLAN_READ_ONLY_TOOLS.contains(&"list_mcp_servers"));
        assert!(!PLAN_READ_ONLY_TOOLS.contains(&"write_file"));
        assert!(!PLAN_READ_ONLY_TOOLS.contains(&"bash"));
        assert!(!PLAN_READ_ONLY_TOOLS.contains(&"call_mcp_tool"));
        assert!(!PLAN_READ_ONLY_TOOLS.contains(&"run_program"));
    }

    #[test]
    fn analyze_tools_are_read_only() {
        assert!(ANALYZE_READ_ONLY_TOOLS.contains(&"read_file"));
        assert!(ANALYZE_READ_ONLY_TOOLS.contains(&"grep_search"));
        assert!(ANALYZE_READ_ONLY_TOOLS.contains(&"code_index"));
        assert!(ANALYZE_READ_ONLY_TOOLS.contains(&"ask_user"));
        assert!(ANALYZE_READ_ONLY_TOOLS.contains(&"choose_option"));
        assert!(ANALYZE_READ_ONLY_TOOLS.contains(&"call_model"));
        assert!(ANALYZE_READ_ONLY_TOOLS.contains(&"read_skill"));
        assert!(ANALYZE_READ_ONLY_TOOLS.contains(&"list_mcp_servers"));
        assert!(!ANALYZE_READ_ONLY_TOOLS.contains(&"write_file"));
        assert!(!ANALYZE_READ_ONLY_TOOLS.contains(&"edit_file"));
        assert!(!ANALYZE_READ_ONLY_TOOLS.contains(&"delete_path"));
        assert!(!ANALYZE_READ_ONLY_TOOLS.contains(&"bash"));
        assert!(!ANALYZE_READ_ONLY_TOOLS.contains(&"execute_code"));
        assert!(!ANALYZE_READ_ONLY_TOOLS.contains(&"run_program"));
        assert!(!ANALYZE_READ_ONLY_TOOLS.contains(&"call_mcp_tool"));
    }

    #[test]
    fn profile_allowlist_filters_read_only_tools() {
        let allowlist = vec!["read_file".to_string(), "bash".to_string()];
        assert_eq!(
            filter_tool_names(PLAN_READ_ONLY_TOOLS, &allowlist),
            vec!["read_file"]
        );
    }
}
