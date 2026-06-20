# yunzhi-one-cli

一个命令，连接所有智能能力。全能 AI 智能体命令行平台。

## 云智 One CLI

`yunzhi` 是一个终端内对话式智能体工具，MVP 采用纯 stdout 流式渲染，核心架构已按后续 ratatui 全屏 TUI 预留：LLM 客户端、Agent Loop、工具系统和权限确认彼此解耦。

## 快速开始

```bash
cargo run -- config set-key sk-xxxx
cargo run
```

也可以首次直接启动：

```bash
cargo run
```

未配置 API Key 时会自动进入引导并保存到 `~/.yunzhi/config.toml`：

```toml
api_key = "sk-xxxx"
```

## 命令

```bash
cargo run -- config show
cargo run -- config set-key sk-xxxx
cargo run -- -p "阅读 README 并总结项目"
cargo run -- print "列出当前目录文件"
cargo run -- --mode plan-act -p "实现一个小功能并验证"
cargo run -- print --mode analyze "分析当前项目结构"
cargo run -- --dangerously-skip-permissions -p "运行 cargo test"
```

安装后启动命令为：

```bash
cargo install --path .
yunzhi
```

## MVP 能力

- 主模型固定为 `Claude-Opus-4.6`，请求固定发送到 `https://yunzhiapi.cn/v1/chat/completions`。
- 支持 chat-completions 风格 `stream: true` SSE 流式响应解析。
- 支持 `--mode` 选择智能体模式，交互中也可以用 `/mode` 查看和切换。
- 内置模式：`chat`、`plan-act`、`entanglement`、`agent`、`team`、`analyze`。
- 预留 `LlmClient` trait，真实接口格式变化时可替换适配层。
- 支持 `read_file`、`write_file`、`edit_file`、`append_file`、`create_dir`、`copy_path`、`move_path`、`delete_path`、`file_info`、`bash`、`execute_code`、`run_program`、`glob_search`、`grep_search`、`list_dir`、`list_models`、`list_skills`、`read_skill`、`list_mcp_servers`、`call_mcp_tool`、`manage_todos`、`system_control`、`call_model`。
- 主模型可以通过 `list_models` 读取云智 API 可用模型列表，并通过 `call_model` 工具调用其他模型完成子任务或交叉检查。
- 支持本地 Skill：启动时索引 `.yunzhi/skills` 与 `~/.yunzhi/skills`，模型可用 `list_skills` 查看技能，用 `read_skill` 读取完整 Markdown 指令后执行。
- 支持 MCP stdio server：读取 `.yunzhi/mcp.json` 与 `~/.yunzhi/mcp.json`，模型可用 `list_mcp_servers` 查看 server，用 `call_mcp_tool` 发起 JSON-RPC 工具调用。
- 写文件、编辑文件、追加文件、复制路径、移动路径、删除路径、执行 bash、执行代码、运行程序和终止进程默认需要确认，支持 `--dangerously-skip-permissions` 跳过。
- `manage_todos` 在当前会话中维护任务列表，支持新增、更新、列出和清空。
- `system_control` 提供受控系统操作：查看工作目录、环境变量、进程列表、磁盘信息和终止进程。
- 启动时读取项目级 `.yunzhi/memory.md` 并注入 system prompt。
- 对话历史保存在内存中，超过阈值后做简单摘要压缩。
- 交互模式支持 `/help`、`/mode`、`/clear`、`/exit`。

## 智能体模式

- `chat`：一次发送一个对话回复，问答、解释和轻量建议优先，默认更克制地使用会改变环境的工具。
- `plan-act`：先规划再执行。交互会话中第一轮只开放只读工具用于读取文件、列目录和搜索；用户输入 `act` 后才恢复写入、执行等工具并开始执行。单次 `print --mode plan-act` 只输出计划。
- `entanglement`：强调上下文联动和交叉检查，适合复杂问题拆解。
- `agent`：默认自主开发模式，需求清楚时直接强制调用工具读写、运行和验证，由工具层负责 diff 和权限确认。
- `team`：主模型担任调度器，先读取可用模型列表，再按架构、实现、测试、审查等角色把任务分配给不同子智能体；一个子智能体完成后，主模型把交付物作为上下文唤醒下一位子智能体。
- `analyze`：只读分析、定位风险和比较方案优先，适合评审和排查。

## Skills 与 MCP

Skill 是可复用的 Markdown 指令文件。项目级 Skill 放在 `.yunzhi/skills`，用户级 Skill 放在 `~/.yunzhi/skills`；可以使用 `name.md`，也可以使用 `name/SKILL.md`。文件开头可写 frontmatter：

```markdown
---
description: Rust 代码审查流程
---
# Rust Review
```

MCP server 使用 JSON 配置。项目级配置为 `.yunzhi/mcp.json`，用户级配置为 `~/.yunzhi/mcp.json`，支持 `mcpServers` 或 `servers` 字段：

```json
{
	"mcpServers": {
		"filesystem": {
			"command": "node",
			"args": ["./mcp-filesystem-server.js"],
			"env": {}
		}
	}
}
```

`call_mcp_tool` 会按 MCP stdio JSON-RPC 初始化 server，并调用 `tools/call`。由于会启动外部进程，默认需要权限确认。

## 设计取舍

当前版本优先交付可通过 `cargo run` 使用的 Agent 核心闭环，因此 UI 先采用 stdout 打字机效果和 ANSI 颜色。`ratatui` 与 `crossterm` 已作为依赖引入，后续可以在不改 Agent Loop 的前提下替换为固定输入框、滚动输出区和状态栏的全屏 TUI。