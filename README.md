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
- 支持 `read_file`、`write_file`、`edit_file`、`append_file`、`create_dir`、`copy_path`、`move_path`、`delete_path`、`file_info`、`bash`、`execute_code`、`run_program`、`glob_search`、`grep_search`、`code_index`、`git_manager`、`test_loop`、`list_dir`、`ask_user`、`choose_option`、`list_models`、`list_skills`、`read_skill`、`list_mcp_servers`、`call_mcp_tool`、`manage_todos`、`system_control`、`call_model`。
- `glob_search`、`grep_search` 与 `code_index` 统一尊重 `.gitignore`、`.git/info/exclude` 和全局 gitignore，并默认跳过 `.git`、`target`、`node_modules`、`dist`、`build` 等大目录以及超大文件，避免把构建产物和依赖目录塞进上下文。
- `code_index` 提供轻量代码理解索引，可按查询返回相关文件、Rust/TypeScript/JavaScript/Python/Go 等常见语言的符号摘要，以及文本引用位置；后续可在同一入口接入 embedding-based 语义检索。
- `git_manager` 提供 Git 原生集成，可查看 status/diff、输出 code review 所需 diff、生成 commit message、创建分支、提交、推送和用 `gh pr create` 打开 PR；会修改仓库或远端的动作默认需要确认。
- `test_loop` 支持测试驱动循环，未提供命令时自动探测 `cargo test`、`npm test`、`pytest`、`go test ./...` 等常见项目测试命令，返回失败摘要供智能体继续修复并重跑。
- 支持一组高层生产力工具：`create_presentation` 制作可转 PPT 的 Marp Markdown 或 PPTX/POTX，`generate_image` 调用绘图/多模态模型生成图片结果，`write_document` 写 Markdown/Word/PDF/ODT/RTF/DOCX/DOTX/EPUB 文档，`write_table` 写 Markdown/CSV/TSV/Excel/XLSX/XLTX/XLS/ODS 表格，`office_document` 统一生成 Word、PPT、Excel、PDF、ODT、RTF、XLSX、ODS、TSV、CSV、XLS、XLTX、DOTX、DOCX、POTX、PPTX、EPUB，`ui_design` 生成 UI 智能设计规格。
- 支持电脑与网络操作：`disk_manager` 管理磁盘用量、大文件和空目录，`computer_manager` 查看/打开/运行电脑任务，`computer_info` 获取系统、CPU、内存、磁盘、网络和环境信息，`web_search` 拉取网络搜索结果，`browser` 获取或打开网页，`network_logs` 获取连接、路由、DNS、ping 和响应头信息。
- 支持数据与记忆操作：`database_manager` 调用 sqlite/psql/mysql/redis-cli 查询或管理数据库，`long_memory` 读取、追加、替换或清空项目长期记忆 `.yunzhi/memory.md`。
- 主模型可以通过 `list_models` 读取云智 API 可用模型列表，并通过 `call_model` 工具调用其他模型完成子任务或交叉检查。
- 支持本地 Skill：启动时索引 `.yunzhi/skills` 与 `~/.yunzhi/skills`，模型可用 `list_skills` 查看技能，用 `read_skill` 读取完整 Markdown 指令后执行。
- 支持 MCP stdio server：读取 `.yunzhi/mcp.json` 与 `~/.yunzhi/mcp.json`，模型可用 `list_mcp_servers` 查看 server，用 `call_mcp_tool` 发起 JSON-RPC 工具调用。
- 写文件、编辑文件、追加文件、复制路径、移动路径、删除路径、执行 bash、执行代码、运行程序、运行测试循环和终止进程默认需要确认，支持彩色 diff 预览；文本写入类工具可输入 `p` 按 hunk 编号选择性应用，或用 `--dangerously-skip-permissions` 跳过。
- `bash`、`execute_code` 和 `test_loop` 默认在临时工作区副本中运行，副本尊重 `.gitignore` 和大文件过滤，命令产生的写入会随沙箱删除；确需操作当前工作区时可显式传 `sandbox=false`。
- 支持项目 Hook：读取 `.yunzhi/hooks.toml`，在工具调用前后执行命令，可用于 pre-commit lint、post-edit format 等自动化。
- 支持配置 Profile：通过 `--profile <name>` 读取 `.yunzhi/profiles.toml` 或 `~/.yunzhi/profiles.toml`，按项目覆盖人格、模式、模型、token 上限和工具白名单。
- 支持 Session 管理：交互模式可保存、恢复、分享会话，并创建 checkpoint、rollback 到某一步的文件快照。
- 支持可观测性与成本估算：每轮结束状态栏展示本轮和会话累计耗时、请求数、估算 token 与估算费用。
- 支持工具审计日志：每次工具调用都会追加到 `.yunzhi/audit/tools.jsonl`，记录工具名、输入、输出摘要、耗时和成功状态，便于回溯 AI 做过什么。
- `manage_todos` 在当前会话中维护任务列表，支持新增、更新、列出和清空。
- `ask_user` 支持 AI 在信息不足时向用户提问并读取自由文本回答；`choose_option` 支持 AI 给出候选项并让用户选择，也可允许自定义答案。
- `system_control` 提供受控系统操作：查看工作目录、环境变量、进程列表、磁盘信息和终止进程。
- 启动时读取项目级 `.yunzhi/memory.md` 并注入 system prompt。
- 对话历史保存在内存中，超过阈值后做简单摘要压缩。
- 交互模式支持 `/help`、`/mode`、`/clear`、`/session`、`/exit`。

## 智能体模式

- `chat`：一次发送一个对话回复，问答、解释和轻量建议优先，默认更克制地使用会改变环境的工具。
- `plan-act`：先规划再执行。交互会话中第一轮只开放只读工具用于读取文件、列目录和搜索；用户输入 `act` 后才恢复写入、执行等工具并开始执行。单次 `print --mode plan-act` 只输出计划。
- `entanglement`：纠缠协作模式。每轮先建立目标、代码、工具、Skills、MCP、模型和未知数的纠缠图，再用只读工具求证、用 `call_model` 做反证/交叉检查，最后输出已确认事实、剩余纠缠点和下一位被唤醒的上下文或子模型。
- `agent`：默认自主开发模式，需求清楚时直接强制调用工具读写、运行和验证，由工具层负责 diff 和权限确认。
- `team`：主模型担任调度器，先读取可用模型列表，再按架构、实现、测试、审查等角色把任务分配给不同子智能体；一个子智能体完成后，主模型把交付物作为上下文唤醒下一位子智能体。
- `analyze`：严格只读分析模式。代码层只开放读取、搜索、文件信息、Skill/MCP 列表和 `call_model` 交叉审查，不提供写入、删除、执行命令、运行程序或 `call_mcp_tool`；输出问题定位、证据链、风险等级、影响范围、方案比较和验证建议。

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

## Hooks、Profiles 与 Sessions

Hook 配置放在 `.yunzhi/hooks.toml`。`pre_tool` 失败会阻止工具调用，`post_tool` 失败会把诊断追加到工具结果中；命令环境变量包含 `YUNZHI_HOOK_EVENT`、`YUNZHI_TOOL_NAME`、`YUNZHI_TOOL_INPUT` 和可选的 `YUNZHI_TOOL_OUTPUT`。

```toml
[[hooks]]
event = "post_tool"
tools = ["write_file", "edit_file", "append_file"]
command = "cargo fmt"
timeout = 60
```

Profile 配置可放在项目级 `.yunzhi/profiles.toml`，也可放在用户级 `~/.yunzhi/profiles.toml`。项目级优先。

```toml
[profiles.rust]
persona = "你是严格的 Rust 工程 reviewer，优先小步修改、测试和清晰错误处理。"
mode = "agent"
model = "Claude-Opus-4.6"
max_tokens = 4096
tools = ["read_file", "grep_search", "code_index", "edit_file", "test_loop", "git_manager"]
```

启动时使用 `yunzhi --profile rust` 或 `yunzhi --profile rust print "修复测试"`。

Session 命令只在交互模式中可用，数据保存在 `.yunzhi/sessions/`。

```text
/session list
/session save <name>
/session resume <name>
/session share <name>
/session checkpoint <name> [note]
/session rollback <name> <checkpoint>
```

## 可观测性与审计

状态栏会在每轮模型响应结束后输出本轮和会话累计指标：耗时、请求数、估算 token、估算费用和当前上下文 token。当前 token 与费用来自本地字符级估算和内置模型单价表；如果上游 API 后续返回官方 usage，可以在同一模块替换为精确计费。

工具调用审计日志采用 JSONL，路径为 `.yunzhi/audit/tools.jsonl`。每行包含 `timestamp_unix`、`call_id`、`tool_name`、`input`、`output_preview`、`is_error` 和 `elapsed_ms`。输出会截断为摘要，避免日志过度膨胀。

## 设计取舍

当前版本优先交付可通过 `cargo run` 使用的 Agent 核心闭环，因此 UI 先采用 stdout 打字机效果和 ANSI 颜色。`ratatui` 与 `crossterm` 已作为依赖引入，后续可以在不改 Agent Loop 的前提下替换为固定输入框、滚动输出区和状态栏的全屏 TUI。
