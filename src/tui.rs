use crate::agent::Agent;
use crate::llm::AnthropicLikeClient;
use crate::llm::DEFAULT_MODEL;
use crate::observability::{TurnMetrics, UsageMetrics};
use crate::tools::{
    PermissionDecision, PermissionPrompter, PermissionRequest, UserChoiceRequest,
    UserChoiceResponse, UserQuestionRequest,
};
use crate::types::AgentMode;
use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::style::Stylize;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;
use std::fs;
use std::io::{self, Write};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, Duration};
use unicode_width::UnicodeWidthChar;

static EVENT_SENDER: OnceLock<Mutex<Option<mpsc::UnboundedSender<TuiEvent>>>> = OnceLock::new();

pub enum TuiEvent {
    User(String),
    AgentDelta(String),
    ToolStart {
        name: String,
        summary: String,
    },
    ToolDone {
        success: bool,
        elapsed_secs: f32,
    },
    Status {
        turn: TurnMetrics,
        session: UsageMetrics,
        context_tokens: usize,
        model: String,
    },
    Warning(String),
    Info(String),
    Error(String),
    Permission {
        request: PermissionRequest,
        respond: oneshot::Sender<PermissionDecision>,
    },
    AskUser {
        request: UserQuestionRequest,
        respond: oneshot::Sender<String>,
    },
    ChooseOption {
        request: UserChoiceRequest,
        respond: oneshot::Sender<UserChoiceResponse>,
    },
}

pub fn install_event_sender(sender: mpsc::UnboundedSender<TuiEvent>) {
    let slot = EVENT_SENDER.get_or_init(|| Mutex::new(None));
    *slot.lock().expect("tui event sender poisoned") = Some(sender);
}

pub fn clear_event_sender() {
    if let Some(slot) = EVENT_SENDER.get() {
        *slot.lock().expect("tui event sender poisoned") = None;
    }
}

pub fn emit_event(event: TuiEvent) -> bool {
    EVENT_SENDER
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|guard| guard.as_ref().cloned()))
        .is_some_and(|sender| sender.send(event).is_ok())
}

pub fn print_banner(version: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    println!("{}", "云智 One".bold().cyan());
    println!("{} v{}", "Yunzhi One CLI".bold(), version);
    println!("当前目录: {}", cwd.display());
    println!("输入 /help 查看命令，/exit 退出。\n");
    Ok(())
}

pub fn print_help() {
    println!("可用命令:");
    println!("  /help   显示帮助");
    println!("  /clear  清空当前对话上下文");
    println!("  /mode   查看可选模式");
    println!("  /mode <模式>  切换模式");
    println!("  /session help  查看会话保存、恢复、checkpoint 和 rollback 命令");
    println!("  /exit   退出");
    println!("快捷键: Ctrl+C 中断当前输入，Ctrl+D 退出。\n");
}

pub fn print_modes(current: AgentMode) {
    println!("当前模式: {}", current);
    println!("可选模式:");
    for mode in AgentMode::ALL {
        let marker = if mode == current { "*" } else { " " };
        println!("  {} {}", marker, mode);
    }
    println!("\n用法: /mode chat 或 yunzhi --mode plan-act\n");
}

pub fn print_user(text: &str) {
    if emit_event(TuiEvent::User(text.to_string())) {
        return;
    }
    println!("{} {}", ">".bold(), text);
}

pub fn print_agent_delta(text: &str) -> Result<()> {
    if emit_event(TuiEvent::AgentDelta(text.to_string())) {
        return Ok(());
    }
    print!("{}", text);
    io::stdout().flush()?;
    Ok(())
}

pub fn print_agent_done(
    turn: TurnMetrics,
    session: UsageMetrics,
    context_tokens: usize,
    model: &str,
) {
    if emit_event(TuiEvent::Status {
        turn,
        session,
        context_tokens,
        model: model.to_string(),
    }) {
        return;
    }
    println!(
        "\n{} 模型 {} | 本轮 {:.1}s | req {} | tokens {} | ${:.6} | 会话 {:.1}s | req {} | tokens {} | ${:.6} | 上下文 {}\n",
        "状态".dark_grey(),
        model,
        turn.elapsed_ms as f32 / 1000.0,
        turn.request_count,
        turn.total_tokens(),
        turn.estimated_cost_usd,
        session.elapsed_ms as f32 / 1000.0,
        session.request_count,
        session.total_tokens(),
        session.estimated_cost_usd,
        context_tokens
    );
}

pub fn print_tool_start(name: &str, summary: &str) {
    if emit_event(TuiEvent::ToolStart {
        name: name.to_string(),
        summary: summary.to_string(),
    }) {
        return;
    }
    println!("{} 调用工具 {}", "●".yellow(), name.bold());
    if !summary.is_empty() {
        println!("└ {}", summary);
    }
}

pub fn print_tool_done(success: bool, elapsed_secs: f32) {
    if emit_event(TuiEvent::ToolDone {
        success,
        elapsed_secs,
    }) {
        return;
    }
    let mark = if success {
        "✓ 完成".green()
    } else {
        "✗ 失败".red()
    };
    println!("└ {} ({:.1}s)\n", mark, elapsed_secs);
}

pub fn print_unverified_completion_warning() {
    if emit_event(TuiEvent::Warning(
        "模型声称操作完成，但本轮没有检测到工具调用；文件或命令可能没有真正执行。".to_string(),
    )) {
        return;
    }
    println!(
        "\n{} 模型声称操作完成，但本轮没有检测到工具调用；文件或命令可能没有真正执行。\n",
        "警告".yellow().bold()
    );
}

pub struct StdoutPrompter;

pub struct EventPrompter;

#[async_trait]
impl PermissionPrompter for EventPrompter {
    async fn confirm(&self, request: PermissionRequest) -> Result<PermissionDecision> {
        let (respond, receive) = oneshot::channel();
        let fallback = request.clone();
        if emit_event(TuiEvent::Permission { request, respond }) {
            return Ok(receive.await?);
        }
        StdoutPrompter.confirm(fallback).await
    }

    async fn ask_user(&self, request: UserQuestionRequest) -> Result<String> {
        let (respond, receive) = oneshot::channel();
        let fallback = request.clone();
        if emit_event(TuiEvent::AskUser { request, respond }) {
            return Ok(receive.await?);
        }
        StdoutPrompter.ask_user(fallback).await
    }

    async fn choose_option(&self, request: UserChoiceRequest) -> Result<UserChoiceResponse> {
        let (respond, receive) = oneshot::channel();
        let fallback = request.clone();
        if emit_event(TuiEvent::ChooseOption { request, respond }) {
            return Ok(receive.await?);
        }
        StdoutPrompter.choose_option(fallback).await
    }
}

#[async_trait]
impl PermissionPrompter for StdoutPrompter {
    async fn confirm(&self, request: PermissionRequest) -> Result<PermissionDecision> {
        println!("{} {}", "需要确认".yellow().bold(), request.tool_name);
        println!("{}", request.summary);
        if let Some(diff) = request.diff {
            println!("{}", "--- diff ---".dark_grey());
            for line in diff.lines() {
                if line.starts_with('+') {
                    println!("{}", line.green());
                } else if line.starts_with('-') {
                    println!("{}", line.red());
                } else {
                    println!("{}", line);
                }
            }
            println!("{}", "------------".dark_grey());
        }
        loop {
            print!("确认执行? [y] 是 / [p] 选择 diff 块 / [n] 否 / [a] 全部允许: ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            match input.trim().to_lowercase().as_str() {
                "y" | "yes" => return Ok(PermissionDecision::Allow),
                "a" | "all" => return Ok(PermissionDecision::AllowAll),
                "p" | "partial" => {
                    print!("请输入要应用的 hunk 编号，例如 1,3-5: ");
                    io::stdout().flush()?;
                    let mut hunk_input = String::new();
                    io::stdin().read_line(&mut hunk_input)?;
                    match parse_hunk_selection(&hunk_input) {
                        Ok(selected) if !selected.is_empty() => {
                            return Ok(PermissionDecision::Partial(selected));
                        }
                        _ => println!("请输入有效的 hunk 编号。"),
                    }
                }
                "n" | "no" => return Ok(PermissionDecision::Deny),
                _ => println!("请输入 y、p、n 或 a。"),
            }
        }
    }

    async fn ask_user(&self, request: UserQuestionRequest) -> Result<String> {
        println!("{}", "需要用户输入".yellow().bold());
        if let Some(context) = request.context {
            println!("{}", context);
        }
        loop {
            match &request.default_answer {
                Some(default_answer) => print!("{} [{}]: ", request.question, default_answer),
                None => print!("{}: ", request.question),
            }
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let answer = input.trim().to_string();
            if !answer.is_empty() {
                return Ok(answer);
            }
            if let Some(default_answer) = &request.default_answer {
                return Ok(default_answer.clone());
            }
            println!("请输入答案。")
        }
    }

    async fn choose_option(&self, request: UserChoiceRequest) -> Result<UserChoiceResponse> {
        println!("{}", "需要用户选择".yellow().bold());
        if let Some(context) = request.context {
            println!("{}", context);
        }
        println!("{}", request.question);
        for (index, option) in request.options.iter().enumerate() {
            println!("  {}. {}", index + 1, option);
        }
        if request.allow_custom {
            println!("也可以直接输入自定义答案。")
        }
        loop {
            print!("请选择 [1-{}]: ", request.options.len());
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let answer = input.trim();
            if let Ok(choice) = answer.parse::<usize>() {
                if (1..=request.options.len()).contains(&choice) {
                    return Ok(UserChoiceResponse {
                        answer: request.options[choice - 1].clone(),
                        index: Some(choice - 1),
                        custom: false,
                    });
                }
            }
            if request.allow_custom && !answer.is_empty() {
                return Ok(UserChoiceResponse {
                    answer: answer.to_string(),
                    index: None,
                    custom: true,
                });
            }
            println!("请输入 1 到 {} 之间的序号。", request.options.len());
        }
    }
}

pub fn ratatui_plan() -> &'static str {
    "全屏 TUI 已启用：固定多行输入框、可滚动输出区、状态栏、历史记录和命令补全。"
}

pub async fn run_fullscreen(
    agent: Agent<AnthropicLikeClient>,
    version: &'static str,
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    install_event_sender(event_tx);
    tokio::spawn(agent_worker(agent, command_rx));

    let mut terminal = FullscreenTerminal::enter()?;
    let mut app = FullscreenApp::new(version);
    let mut tick = time::interval(Duration::from_millis(50));

    loop {
        terminal.draw(&mut app)?;
        tokio::select! {
            _ = tick.tick() => {
                while event::poll(Duration::from_millis(0))? {
                    match event::read()? {
                        Event::Key(key) => {
                            if handle_key(key, &mut app, &command_tx)? {
                                let _ = command_tx.send(AgentCommand::Shutdown);
                                clear_event_sender();
                                return Ok(());
                            }
                        }
                        Event::Mouse(mouse) => handle_mouse(mouse, &mut app),
                        Event::Paste(text) => app.insert_text_at_cursor(&text),
                        _ => {}
                    }
                }
            }
            Some(event) = event_rx.recv() => {
                app.apply_event(event, &command_tx);
            }
        }
    }
}

struct FullscreenTerminal {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl FullscreenTerminal {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }

    fn draw(&mut self, app: &mut FullscreenApp) -> Result<()> {
        self.terminal.draw(|frame| app.render(frame))?;
        Ok(())
    }
}

impl Drop for FullscreenTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableBracketedPaste
        );
        let _ = self.terminal.show_cursor();
    }
}

enum AgentCommand {
    Submit(String),
    Clear,
    SetMode(String),
    SetModel(String),
    Session(String),
    Shutdown,
}

async fn agent_worker(
    mut agent: Agent<AnthropicLikeClient>,
    mut commands: mpsc::UnboundedReceiver<AgentCommand>,
) {
    let mut last_request: Option<String> = None;
    emit_event(TuiEvent::Info(format!(
        "当前模式: {} | 当前模型: {}",
        agent.mode(),
        agent.model()
    )));
    while let Some(command) = commands.recv().await {
        match command {
            AgentCommand::Shutdown => break,
            AgentCommand::Clear => match agent.clear() {
                Ok(()) => {
                    emit_event(TuiEvent::Info("上下文已清空。".to_string()));
                }
                Err(error) => {
                    emit_event(TuiEvent::Error(format!("清空上下文失败: {error:#}")));
                }
            },
            AgentCommand::SetMode(raw_mode) => match AgentMode::from_str(raw_mode.trim()) {
                Ok(mode) => match agent.set_mode(mode) {
                    Ok(()) => {
                        emit_event(TuiEvent::Info(format!("已切换到 {} 模式。", agent.mode())));
                    }
                    Err(error) => {
                        emit_event(TuiEvent::Error(format!("切换模式失败: {error:#}")));
                    }
                },
                Err(error) => {
                    emit_event(TuiEvent::Error(format!("错误: {error}")));
                }
            },
            AgentCommand::SetModel(raw_model) => match agent.set_model(raw_model) {
                Ok(()) => {
                    emit_event(TuiEvent::Info(format!(
                        "已切换主控模型: {}。",
                        agent.model()
                    )));
                    emit_event(TuiEvent::Info(format!("当前模型: {}", agent.model())));
                }
                Err(error) => {
                    emit_event(TuiEvent::Error(format!("切换模型失败: {error:#}")));
                }
            },
            AgentCommand::Session(command) => {
                if let Err(error) = handle_fullscreen_session_command(&mut agent, command.trim()) {
                    emit_event(TuiEvent::Error(format!("错误: {error:#}")));
                }
            }
            AgentCommand::Submit(input) => {
                let turn_input = if is_confirmation(&input) {
                    last_request
                        .as_ref()
                        .map(|request| format!("确认执行上一条请求: {request}"))
                        .unwrap_or_else(|| input.clone())
                } else {
                    last_request = Some(input.clone());
                    input
                };
                if let Err(error) = agent.run_turn(turn_input).await {
                    emit_event(TuiEvent::Error(format!("错误: {error:#}")));
                }
            }
        };
    }
}

struct FullscreenApp {
    version: &'static str,
    lines: Vec<LogLine>,
    input: Vec<char>,
    cursor: usize,
    scroll: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    status: String,
    busy: bool,
    pending: Option<PendingPrompt>,
    output_area: Rect,
}

impl FullscreenApp {
    fn new(version: &'static str) -> Self {
        Self {
            version,
            lines: vec![LogLine::info(format!(
                "云智 One v{version}。输入 /help 查看命令，Enter 发送，Ctrl+J 换行，支持系统粘贴，Ctrl+C 退出。"
            ))],
            input: Vec::new(),
            cursor: 0,
            scroll: 0,
            history: Vec::new(),
            history_index: None,
            status: format!("模式 agent | 模型 {DEFAULT_MODEL} | tokens 0 | $0.000000"),
            busy: false,
            pending: None,
            output_area: Rect::default(),
        }
    }

    fn render(&mut self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(1),
                Constraint::Length(5),
            ])
            .split(area);
        self.output_area = chunks[0];

        let output_items = self
            .lines
            .iter()
            .flat_map(LogLine::to_items)
            .collect::<Vec<_>>();
        let visible_height = chunks[0].height.saturating_sub(2) as usize;
        let max_scroll = output_items.len().saturating_sub(visible_height);
        self.scroll = self.scroll.min(max_scroll);
        let visible_end = output_items.len().saturating_sub(self.scroll);
        let visible_start = visible_end.saturating_sub(visible_height);
        let position = if self.scroll == 0 {
            "底部".to_string()
        } else {
            format!("上移 {} 行", self.scroll)
        };
        let output_title = format!(
            " 云智 One v{} | 输出 | {} | 滚轮/PageUp/PageDown ",
            self.version, position
        );
        let output = List::new(
            output_items
                .into_iter()
                .skip(visible_start)
                .take(visible_end.saturating_sub(visible_start))
                .collect::<Vec<_>>(),
        )
        .block(Block::default().title(output_title).borders(Borders::ALL))
        .style(Style::default().fg(Color::Gray))
        .highlight_style(Style::default().fg(Color::White));
        frame.render_widget(output, chunks[0]);

        let status = Paragraph::new(self.status.clone())
            .style(Style::default().bg(Color::Rgb(34, 40, 49)).fg(Color::White));
        frame.render_widget(status, chunks[1]);

        let input_title = if self.busy {
            " 输入 | 运行中 | Enter 发送 Ctrl+J 换行 "
        } else {
            " 输入 | Enter 发送 Ctrl+J 换行 "
        };
        let input = Paragraph::new(self.input_string())
            .style(Style::default().fg(Color::White))
            .block(
                Block::default()
                    .title(input_title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(input, chunks[2]);
        let (cursor_x, cursor_y) = self.cursor_position(chunks[2]);
        frame.set_cursor_position((cursor_x, cursor_y));

        if let Some(completions) = self.completions() {
            let popup = completion_area(area, completions.len() as u16);
            frame.render_widget(Clear, popup);
            frame.render_widget(
                List::new(
                    completions
                        .into_iter()
                        .map(|item| ListItem::new(item).style(Style::default().fg(Color::Cyan)))
                        .collect::<Vec<_>>(),
                )
                .block(Block::default().title(" 补全 ").borders(Borders::ALL)),
                popup,
            );
        }

        if let Some(pending) = &self.pending {
            let popup = centered_rect(78, 60, area);
            frame.render_widget(Clear, popup);
            let text = Text::from(pending.render_text());
            frame.render_widget(
                Paragraph::new(text)
                    .style(Style::default().fg(Color::White))
                    .block(
                        Block::default()
                            .title(" 需要确认 ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow)),
                    )
                    .wrap(Wrap { trim: false }),
                popup,
            );
        }
    }

    fn apply_event(&mut self, event: TuiEvent, command_tx: &mpsc::UnboundedSender<AgentCommand>) {
        match event {
            TuiEvent::User(text) => self.push(LogLine::user(text)),
            TuiEvent::AgentDelta(delta) => self.push_agent_delta(&delta),
            TuiEvent::ToolStart { name, summary } => {
                self.busy = true;
                self.push(LogLine::tool(format!("调用工具 {name}")));
                if !summary.is_empty() {
                    self.push(LogLine::dim(pretty_json_or_raw(&summary)));
                }
            }
            TuiEvent::ToolDone {
                success,
                elapsed_secs,
            } => {
                self.busy = false;
                self.push(LogLine::tool(format!(
                    "{} ({elapsed_secs:.1}s)",
                    if success { "完成" } else { "失败" }
                )));
            }
            TuiEvent::Status {
                turn,
                session,
                context_tokens,
                model,
            } => {
                self.busy = false;
                self.status = format!(
                    "模型 {} | 本轮 {:.1}s req {} tokens {} ${:.6} | 会话 {:.1}s req {} tokens {} ${:.6} | 上下文 {}",
                    model,
                    turn.elapsed_ms as f32 / 1000.0,
                    turn.request_count,
                    turn.total_tokens(),
                    turn.estimated_cost_usd,
                    session.elapsed_ms as f32 / 1000.0,
                    session.request_count,
                    session.total_tokens(),
                    session.estimated_cost_usd,
                    context_tokens
                );
                self.push(LogLine::dim("状态已更新"));
            }
            TuiEvent::Warning(message) => self.push(LogLine::warn(message)),
            TuiEvent::Info(message) => self.push(LogLine::info(message)),
            TuiEvent::Error(message) => {
                self.busy = false;
                self.push(LogLine::error(message));
            }
            TuiEvent::Permission { request, respond } => {
                self.pending = Some(PendingPrompt::Permission { request, respond });
            }
            TuiEvent::AskUser { request, respond } => {
                self.pending = Some(PendingPrompt::Ask { request, respond });
            }
            TuiEvent::ChooseOption { request, respond } => {
                self.pending = Some(PendingPrompt::Choice { request, respond });
            }
        }
        let _ = command_tx;
    }

    fn submit_input(&mut self, command_tx: &mpsc::UnboundedSender<AgentCommand>) {
        let input = self.input_string().trim().to_string();
        if input.is_empty() {
            return;
        }
        if let Some(pending) = self.pending.take() {
            self.answer_pending(pending, input);
            return;
        }
        self.history.push(input.clone());
        self.history_index = None;
        self.input.clear();
        self.cursor = 0;
        match input.as_str() {
            "/exit" => {}
            "/help" => self.push(LogLine::info(help_text())),
            "/clear" => {
                let _ = command_tx.send(AgentCommand::Clear);
            }
            "/mode" => self.push(LogLine::info(mode_text())),
            _ if input.starts_with("/mode ") => {
                let _ = command_tx.send(AgentCommand::SetMode(input[6..].to_string()));
            }
            "/model" => self.push(LogLine::info(model_text())),
            _ if input.starts_with("/model ") => {
                let _ = command_tx.send(AgentCommand::SetModel(input[7..].to_string()));
            }
            _ if input.starts_with("/session") => {
                let _ = command_tx.send(AgentCommand::Session(input[8..].trim().to_string()));
            }
            _ => {
                self.push(LogLine::user(input.clone()));
                self.busy = true;
                let _ = command_tx.send(AgentCommand::Submit(input));
            }
        }
    }

    fn answer_pending(&mut self, pending: PendingPrompt, input: String) {
        match pending {
            PendingPrompt::Permission { request, respond } => {
                let decision = match input.trim().to_ascii_lowercase().as_str() {
                    "y" | "yes" => PermissionDecision::Allow,
                    "a" | "all" => PermissionDecision::AllowAll,
                    "n" | "no" => PermissionDecision::Deny,
                    raw if raw.starts_with('p') => {
                        let selection = raw.trim_start_matches('p').trim();
                        match parse_hunk_selection(selection) {
                            Ok(selected) if !selected.is_empty() => {
                                PermissionDecision::Partial(selected)
                            }
                            _ => {
                                self.push(LogLine::warn("请输入 y、a、n 或 p 1,3-5。"));
                                self.pending = Some(PendingPrompt::Permission { request, respond });
                                return;
                            }
                        }
                    }
                    _ => {
                        self.push(LogLine::warn("请输入 y、a、n 或 p 1,3-5。"));
                        self.pending = Some(PendingPrompt::Permission { request, respond });
                        return;
                    }
                };
                let _ = respond.send(decision);
            }
            PendingPrompt::Ask { request, respond } => {
                if input.trim().is_empty() {
                    if let Some(default_answer) = request.default_answer {
                        let _ = respond.send(default_answer);
                    } else {
                        self.push(LogLine::warn("请输入答案。"));
                        self.pending = Some(PendingPrompt::Ask { request, respond });
                    }
                } else {
                    let _ = respond.send(input);
                }
            }
            PendingPrompt::Choice { request, respond } => {
                let answer = input.trim();
                if let Ok(choice) = answer.parse::<usize>() {
                    if (1..=request.options.len()).contains(&choice) {
                        let _ = respond.send(UserChoiceResponse {
                            answer: request.options[choice - 1].clone(),
                            index: Some(choice - 1),
                            custom: false,
                        });
                        return;
                    }
                }
                if request.allow_custom && !answer.is_empty() {
                    let _ = respond.send(UserChoiceResponse {
                        answer: answer.to_string(),
                        index: None,
                        custom: true,
                    });
                } else {
                    self.push(LogLine::warn(format!(
                        "请输入 1 到 {} 之间的序号。",
                        request.options.len()
                    )));
                    self.pending = Some(PendingPrompt::Choice { request, respond });
                }
            }
        }
    }

    fn push(&mut self, line: LogLine) {
        let follow_tail = self.scroll == 0;
        self.lines.push(line);
        if self.lines.len() > 2000 {
            self.lines.drain(..500);
        }
        if follow_tail {
            self.scroll = 0;
        }
    }

    fn push_agent_delta(&mut self, delta: &str) {
        let follow_tail = self.scroll == 0;
        if !matches!(
            self.lines.last(),
            Some(LogLine {
                kind: LogKind::Agent,
                ..
            })
        ) {
            self.lines.push(LogLine::agent(String::new()));
        }
        if let Some(line) = self.lines.last_mut() {
            line.text.push_str(delta);
        }
        if follow_tail {
            self.scroll = 0;
        }
    }

    fn scroll_up(&mut self, lines: u16) {
        self.scroll = self.scroll.saturating_add(lines as usize);
    }

    fn scroll_down(&mut self, lines: u16) {
        self.scroll = self.scroll.saturating_sub(lines as usize);
    }

    fn scroll_to_top(&mut self) {
        self.scroll = usize::MAX;
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }

    fn insert_text_at_cursor(&mut self, text: &str) {
        let inserted = text.chars().collect::<Vec<_>>();
        let inserted_len = inserted.len();
        self.input.splice(self.cursor..self.cursor, inserted);
        self.cursor += inserted_len;
    }

    fn input_string(&self) -> String {
        self.input.iter().collect()
    }

    fn cursor_position(&self, area: Rect) -> (u16, u16) {
        let before = self.input.iter().take(self.cursor).collect::<String>();
        let mut x = 0_u16;
        let mut y = 0_u16;
        let width = area.width.saturating_sub(2).max(1);
        for ch in before.chars() {
            if ch == '\n' {
                x = 0;
                y = y.saturating_add(1);
            } else {
                let char_width = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                if char_width == 0 {
                    continue;
                }
                if x.saturating_add(char_width) > width {
                    x = 0;
                    y = y.saturating_add(1);
                }
                x = x.saturating_add(char_width);
                if x >= width {
                    x = 0;
                    y = y.saturating_add(1);
                }
            }
        }
        (
            area.x + 1 + x,
            area.y + 1 + y.min(area.height.saturating_sub(3)),
        )
    }

    fn completions(&self) -> Option<Vec<String>> {
        let input = self.input_string();
        let token = input.split_whitespace().last().unwrap_or(input.as_str());
        if token.starts_with('/') {
            let options = command_completions()
                .into_iter()
                .filter(|item| item.starts_with(token))
                .map(str::to_string)
                .collect::<Vec<_>>();
            (!options.is_empty()).then_some(options)
        } else if let Some(prefix) = token.strip_prefix('@') {
            let options = file_completions(prefix);
            (!options.is_empty()).then_some(options)
        } else {
            None
        }
    }
}

fn handle_key(
    key: KeyEvent,
    app: &mut FullscreenApp,
    command_tx: &mpsc::UnboundedSender<AgentCommand>,
) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }
    match key.code {
        KeyCode::Enter => app.submit_input(command_tx),
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.insert(app.cursor, '\n');
            app.cursor += 1;
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => app.scroll_up(5),
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => app.scroll_down(5),
        KeyCode::Char(ch) => {
            app.insert_text_at_cursor(&ch.to_string());
        }
        KeyCode::Backspace => {
            if app.cursor > 0 {
                app.cursor -= 1;
                app.input.remove(app.cursor);
            }
        }
        KeyCode::Delete => {
            if app.cursor < app.input.len() {
                app.input.remove(app.cursor);
            }
        }
        KeyCode::Left => app.cursor = app.cursor.saturating_sub(1),
        KeyCode::Right => app.cursor = (app.cursor + 1).min(app.input.len()),
        KeyCode::Up => {
            if !app.history.is_empty() {
                let index = app
                    .history_index
                    .unwrap_or(app.history.len())
                    .saturating_sub(1);
                app.history_index = Some(index);
                app.input = app.history[index].chars().collect();
                app.cursor = app.input.len();
            }
        }
        KeyCode::Down => {
            if let Some(index) = app.history_index {
                if index + 1 < app.history.len() {
                    app.history_index = Some(index + 1);
                    app.input = app.history[index + 1].chars().collect();
                } else {
                    app.history_index = None;
                    app.input.clear();
                }
                app.cursor = app.input.len();
            }
        }
        KeyCode::PageUp => app.scroll_up(10),
        KeyCode::PageDown => app.scroll_down(10),
        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => app.scroll_to_top(),
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => app.scroll_to_bottom(),
        KeyCode::Tab => apply_completion(app),
        _ => {}
    }
    Ok(false)
}

fn handle_mouse(mouse: MouseEvent, app: &mut FullscreenApp) {
    if !contains_point(app.output_area, mouse.column, mouse.row) {
        return;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => app.scroll_up(3),
        MouseEventKind::ScrollDown => app.scroll_down(3),
        _ => {}
    }
}

fn contains_point(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

#[derive(Clone)]
struct LogLine {
    kind: LogKind,
    text: String,
}

#[derive(Clone)]
enum LogKind {
    User,
    Agent,
    Tool,
    Info,
    Warn,
    Error,
    Dim,
}

impl LogLine {
    fn user(text: String) -> Self {
        Self {
            kind: LogKind::User,
            text,
        }
    }
    fn agent(text: String) -> Self {
        Self {
            kind: LogKind::Agent,
            text,
        }
    }
    fn tool(text: String) -> Self {
        Self {
            kind: LogKind::Tool,
            text,
        }
    }
    fn info(text: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Info,
            text: text.into(),
        }
    }
    fn warn(text: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Warn,
            text: text.into(),
        }
    }
    fn error(text: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Error,
            text: text.into(),
        }
    }
    fn dim(text: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Dim,
            text: text.into(),
        }
    }

    fn to_items(&self) -> Vec<ListItem<'static>> {
        let (label, style) = match self.kind {
            LogKind::User => (
                "> ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            LogKind::Agent => ("  ", Style::default().fg(Color::White)),
            LogKind::Tool => ("● ", Style::default().fg(Color::Yellow)),
            LogKind::Info => ("i ", Style::default().fg(Color::Cyan)),
            LogKind::Warn => ("! ", Style::default().fg(Color::Yellow)),
            LogKind::Error => ("x ", Style::default().fg(Color::Red)),
            LogKind::Dim => ("  ", Style::default().fg(Color::DarkGray)),
        };
        self.text
            .lines()
            .chain(if self.text.ends_with('\n') {
                Some("")
            } else {
                None
            })
            .map(|line| {
                ListItem::new(Line::from(vec![
                    Span::styled(label, style),
                    Span::raw(line.to_string()),
                ]))
            })
            .collect()
    }
}

enum PendingPrompt {
    Permission {
        request: PermissionRequest,
        respond: oneshot::Sender<PermissionDecision>,
    },
    Ask {
        request: UserQuestionRequest,
        respond: oneshot::Sender<String>,
    },
    Choice {
        request: UserChoiceRequest,
        respond: oneshot::Sender<UserChoiceResponse>,
    },
}

impl PendingPrompt {
    fn render_text(&self) -> String {
        match self {
            PendingPrompt::Permission { request, .. } => {
                let mut text = format!(
                    "{}\n{}\n\n输入 y 执行，a 全部允许，n 拒绝，p 1,3-5 选择 diff 块。",
                    request.tool_name, request.summary
                );
                if let Some(diff) = &request.diff {
                    text.push_str("\n\n--- diff ---\n");
                    text.push_str(diff);
                }
                text
            }
            PendingPrompt::Ask { request, .. } => {
                let mut text = String::new();
                if let Some(context) = &request.context {
                    text.push_str(context);
                    text.push_str("\n\n");
                }
                text.push_str(&request.question);
                if let Some(default_answer) = &request.default_answer {
                    text.push_str(&format!("\n默认: {default_answer}"));
                }
                text
            }
            PendingPrompt::Choice { request, .. } => {
                let mut text = String::new();
                if let Some(context) = &request.context {
                    text.push_str(context);
                    text.push_str("\n\n");
                }
                text.push_str(&request.question);
                for (index, option) in request.options.iter().enumerate() {
                    text.push_str(&format!("\n{}. {}", index + 1, option));
                }
                if request.allow_custom {
                    text.push_str("\n也可以输入自定义答案。");
                }
                text
            }
        }
    }
}

fn handle_fullscreen_session_command(
    agent: &mut Agent<AnthropicLikeClient>,
    command: &str,
) -> Result<()> {
    let mut parts = command.split_whitespace();
    match parts.next() {
        None | Some("help") => emit_event(TuiEvent::Info(session_help_text())),
        Some("list") => {
            let sessions = agent.list_sessions()?;
            emit_event(TuiEvent::Info(if sessions.is_empty() {
                "暂无已保存会话。".to_string()
            } else {
                sessions.join("\n")
            }))
        }
        Some("save") => {
            let id = parts.next().ok_or_else(|| anyhow::anyhow!("缺少会话名"))?;
            let path = agent.save_session(id)?;
            emit_event(TuiEvent::Info(format!("会话已保存: {}", path.display())))
        }
        Some("resume") => {
            let id = parts.next().ok_or_else(|| anyhow::anyhow!("缺少会话名"))?;
            agent.resume_session(id)?;
            emit_event(TuiEvent::Info(format!("已恢复会话: {id}")))
        }
        Some("share") => {
            let id = parts.next().ok_or_else(|| anyhow::anyhow!("缺少会话名"))?;
            let path = agent.share_session(id)?;
            emit_event(TuiEvent::Info(format!(
                "分享文件已生成: {}",
                path.display()
            )))
        }
        Some("checkpoint") => {
            let id = parts.next().ok_or_else(|| anyhow::anyhow!("缺少会话名"))?;
            let note = parts.collect::<Vec<_>>().join(" ");
            let checkpoint = agent.checkpoint_session(id, (!note.is_empty()).then_some(note))?;
            emit_event(TuiEvent::Info(format!("checkpoint 已创建: {checkpoint}")))
        }
        Some("rollback") => {
            let id = parts.next().ok_or_else(|| anyhow::anyhow!("缺少会话名"))?;
            let checkpoint = parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("缺少 checkpoint id"))?;
            agent.rollback_session(id, checkpoint)?;
            emit_event(TuiEvent::Info(format!("已回滚到 checkpoint: {checkpoint}")))
        }
        Some(other) => emit_event(TuiEvent::Error(format!("未知 /session 子命令: {other}"))),
    };
    Ok(())
}

fn is_confirmation(input: &str) -> bool {
    matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "ok" | "okay" | "确认" | "可以" | "好" | "好的"
    )
}

fn help_text() -> String {
    [
        "/help 显示帮助",
        "/clear 清空当前对话上下文",
        "/mode 查看可选模式",
        "/mode <模式> 切换模式",
        "/model 查看模型切换用法",
        "/model <模型名> 切换本会话主控模型",
        "/session help 查看会话命令",
        "/exit 退出",
        "Enter 发送，Ctrl+J 换行，支持系统粘贴，↑↓ 翻历史，Tab 应用补全。",
        "PageUp/PageDown 或鼠标滚轮滚动输出，Ctrl+Home 顶部，Ctrl+End 底部。",
    ]
    .join("\n")
}

fn mode_text() -> String {
    let modes = AgentMode::ALL
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!("可选模式: {modes}\n用法: /mode chat 或 /mode agent")
}

fn model_text() -> String {
    format!("默认主控模型: {DEFAULT_MODEL}\n用法: /model DeepSeek-V4-pro")
}

fn session_help_text() -> String {
    [
        "/session list",
        "/session save <name>",
        "/session resume <name>",
        "/session share <name>",
        "/session checkpoint <name> [note]",
        "/session rollback <name> <checkpoint>",
    ]
    .join("\n")
}

fn command_completions() -> Vec<&'static str> {
    vec![
        "/help",
        "/clear",
        "/mode",
        "/mode chat",
        "/mode plan-act",
        "/mode agent",
        "/mode team",
        "/mode analyze",
        "/model",
        "/model DeepSeek-V4-pro",
        "/session",
        "/session list",
        "/session save",
        "/session resume",
        "/exit",
    ]
}

fn file_completions(prefix: &str) -> Vec<String> {
    let path = if prefix.is_empty() { "." } else { prefix };
    let (dir, needle) = match path.rsplit_once('/') {
        Some((dir, needle)) => (if dir.is_empty() { "." } else { dir }, needle),
        None => (".", path),
    };
    fs::read_dir(dir)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.starts_with(needle).then(|| {
                let suffix = if entry.path().is_dir() { "/" } else { "" };
                if dir == "." {
                    format!("@{name}{suffix}")
                } else {
                    format!("@{dir}/{name}{suffix}")
                }
            })
        })
        .take(8)
        .collect()
}

fn apply_completion(app: &mut FullscreenApp) {
    let Some(first) = app.completions().and_then(|items| items.into_iter().next()) else {
        return;
    };
    let input = app.input_string();
    let start = input
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(0);
    let prefix_chars = input[..start].chars().collect::<Vec<_>>();
    app.input = prefix_chars.into_iter().chain(first.chars()).collect();
    app.cursor = app.input.len();
}

fn pretty_json_or_raw(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| raw.to_string())
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn completion_area(area: Rect, len: u16) -> Rect {
    Rect {
        x: area.x + 2,
        y: area.y + area.height.saturating_sub(12),
        width: area.width.min(48),
        height: (len + 2).min(10),
    }
}

pub fn parse_hunk_selection(input: &str) -> Result<Vec<usize>> {
    let mut selected = Vec::new();
    for part in input
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((start, end)) = part.split_once('-') {
            let start = start.trim().parse::<usize>()?;
            let end = end.trim().parse::<usize>()?;
            anyhow::ensure!(start <= end, "hunk 范围起点不能大于终点");
            selected.extend(start..=end);
        } else {
            selected.push(part.parse::<usize>()?);
        }
    }
    selected.sort_unstable();
    selected.dedup();
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_mentions_output_scroll_controls() {
        let text = help_text();
        assert!(text.contains("PageUp/PageDown"));
        assert!(text.contains("Ctrl+Home"));
        assert!(text.contains("Ctrl+End"));
    }

    #[test]
    fn paste_inserts_multiline_text_at_cursor() {
        let mut app = FullscreenApp::new("test");
        app.insert_text_at_cursor("hello world");
        app.cursor = 5;
        app.insert_text_at_cursor("\n粘贴\n");

        assert_eq!(app.input_string(), "hello\n粘贴\n world");
        assert_eq!(app.cursor, "hello\n粘贴\n".chars().count());
    }

    #[test]
    fn cursor_position_uses_display_width() {
        let mut app = FullscreenApp::new("test");
        app.insert_text_at_cursor("abc中文");

        assert_eq!(app.cursor_position(Rect::new(0, 0, 20, 5)), (8, 1));
    }

    #[test]
    fn cursor_position_wraps_wide_characters() {
        let mut app = FullscreenApp::new("test");
        app.insert_text_at_cursor("abcd中");

        assert_eq!(app.cursor_position(Rect::new(0, 0, 7, 5)), (3, 2));
    }

    #[test]
    fn contains_point_respects_area_edges() {
        let area = Rect::new(2, 3, 10, 4);

        assert!(contains_point(area, 2, 3));
        assert!(contains_point(area, 11, 6));
        assert!(!contains_point(area, 12, 6));
        assert!(!contains_point(area, 11, 7));
    }
}
