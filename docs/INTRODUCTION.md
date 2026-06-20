# 云智 One CLI — 终端 AI 智能体平台

## 项目概览

云智 One CLI（yunzhi-one-cli）是一个基于 Rust 开发的终端 AI 智能体命令行工具。一个命令 `yunzhi`，连接所有智能能力——它通过调用云智 API（DeepSeek-V4-pro 等模型），在终端内提供文件操作、代码执行、Git 管理、文档生成、网络搜索、数据库管理等数十种工具，并支持 6 种智能体模式（agent、chat、plan-act、entanglement、team、analyze），是开发者和高级用户的终端 AI 中心。

## 核心设计理念

yunzhi 的设计遵循三大原则：

1. **一个命令，所有能力**：不需要在多个工具间切换，所有文件、代码、Git、文档、搜索、数据库操作都在同一个终端对话中完成。
2. **安全先行**：危险操作（bash、文件覆盖、网络请求等）默认需要用户确认并展示彩色 diff 预览；bash 和代码执行默认在沙箱副本中运行，保护工作区。
3. **可扩展**：通过 Skills（可复用 Markdown 指令）、MCP（Model Context Protocol）stdio server 和 Hooks 机制，用户可以无限扩展智能体的能力边界。

## 快速开始

```bash
# 安装
git clone <repo-url> && cd yunzhi-one-cli
cargo install --path .

# 配置 API Key（也可首次直接启动进入引导）
yunzhi config set-key sk-xxxx

# 启动交互模式
yunzhi

# 单次指令
yunzhi -p "阅读 README 并总结项目"

# 指定模型
yunzhi --model DeepSeek-V4-pro -p "列出当前目录文件"

# 指定模式
yunzhi --mode plan-act -p "实现一个小功能并验证"
```

## 技术架构

yunzhi 采用 Rust 2021 edition 编写，tokio 异步运行时驱动，ratatui + crossterm 渲染全屏 TUI 界面。

**项目结构**：
- `main.rs` - 入口，解析 CLI 参数并分发
- `cli.rs` - CLI 命令定义（config、print、skill、mcp、mcp-server）
- `agent.rs` - 核心智能体引擎：模式管理、对话循环、上下文压缩、工具调用编排
- `llm.rs` - LLM 客户端：SSE 流式解析、HTTP 请求、模型列表、重试逻辑
- `tools.rs` - 内置工具集（44+ 工具），含权限确认、diff 预览、沙箱执行
- `tui.rs` - 全屏 TUI：输出区、输入框、状态栏、工具调用可视化
- `types.rs` - 核心类型：消息、工具定义、智能体模式
- `config.rs` - 配置管理：API Key、Profile、长期记忆
- `extensions.rs` - Skills 和 MCP 管理
- `hooks.rs` - 工具调用前后钩子
- `session.rs` - 会话保存/恢复/分享/checkpoint/rollback
- `observability.rs` - token 估算、费用计算、审计日志
- `mcp_server.rs` - MCP stdio server 实现

**关键依赖**：tokio（异步）、ratatui/crossterm（TUI）、reqwest（HTTP）、clap（CLI 解析）、serde_json（序列化）、ignore（gitignore 支持）、zip（办公文档生成）。

## 智能体模式

yunzhi 提供 6 种智能体模式，适应不同工作场景：

| 模式 | 说明 | 适用场景 |
|------|------|----------|
| `agent` | 默认自主开发模式，直接调用工具读写、运行、验证 | 日常开发、需求明确的编码任务 |
| `chat` | 问答模式，优先不改变环境 | 解释代码、轻量建议 |
| `plan-act` | 先规划后执行：第一轮只读探查并输出计划，用户输入 act 后执行 | 复杂功能实现、需要审查计划的任务 |
| `entanglement` | 纠缠协作模式：建立目标/代码/工具/Skills/MCP 纠缠图，交叉验证 | 高风险变更、需要多方验证的决策 |
| `team` | 主模型担任调度器，分配架构/实现/测试/审查子智能体 | 大型项目、需要分角色协作 |
| `analyze` | 严格只读分析：定位问题、证据链、风险评估 | 代码审查、安全性分析 |

交互模式中可用 `/mode` 查看和切换模式。

## 内置工具集

yunzhi 内置 44+ 工具，覆盖软件开发全流程：

**文件操作**（10 个）：read_file、write_file、edit_file、append_file、create_dir、copy_path、move_path、delete_path、file_info、list_dir

**搜索与分析**（3 个）：glob_search、grep_search、code_index（轻量代码索引，支持符号提取和引用追踪）

**命令执行**（3 个）：bash（默认沙箱）、execute_code（python/node/bash/rust）、run_program

**Git 集成**（1 个）：git_manager（status/diff/review_diff/message/create_branch/commit/push/open_pr）

**测试**（1 个）：test_loop（自动探测测试命令，返回失败摘要供迭代修复）

**交互**（3 个）：ask_user、choose_option、manage_todos

**Skills & MCP**（7 个）：list_skills、read_skill、add_skill、list_mcp_servers、add_mcp_server、call_mcp_tool、mcp_resource、mcp_prompt

**生产力**（6 个）：create_presentation、generate_image、write_document、write_table、office_document、ui_design

**系统与网络**（7 个）：system_control、disk_manager、computer_manager、computer_info、web_search、browser、network_logs

**数据与记忆**（3 个）：database_manager（sqlite/postgres/mysql/redis）、long_memory、call_model（委托子模型）

## Skills 可复用指令系统

Skills 是存放在 `.yunzhi/skills`（项目级）或 `~/.yunzhi/skills`（用户级）的 Markdown 指令文件。每个 Skill 可包含 frontmatter 元数据。

```bash
# 从文件添加 Skill
yunzhi skill add code/review --description "Rust 代码审查流程" --file ./review-skill.md

# 直接写入内容
yunzhi skill add writing/ppt --description "PPT 制作流程" --content "按目标、受众、结构和版式输出。"

# 列出和读取
yunzhi skill list
yunzhi skill read code/review
```

AI 在对话中可通过 `list_skills` 查看所有 Skill，按需调用 `read_skill` 读取完整指令后执行。

## MCP 协议支持

yunzhi 同时是 MCP（Model Context Protocol）的 **客户端** 和 **服务端**：

**作为 MCP 客户端**：通过 `.yunzhi/mcp.json` 或 `~/.yunzhi/mcp.json` 配置外部 MCP stdio server，AI 可调用其工具、读取资源和获取提示模板。

```bash
# 添加 MCP server
yunzhi mcp add filesystem node --arg ./server.js

# 列出
yunzhi mcp list
```

**作为 MCP 服务端**：
```bash
yunzhi mcp-server
```

通过 stdio 暴露 yunzhi 的内置工具，可被 VS Code 插件等 MCP client 调用。

## 可观测性与安全

**状态栏**：每轮对话后展示本轮和会话累计耗时、请求数、估算 token 与费用。

**审计日志**：每次工具调用记录到 `.yunzhi/audit/tools.jsonl`，包含工具名、输入、输出摘要、耗时和成功状态。

**沙箱执行**：bash、execute_code 和 test_loop 默认在临时工作区副本中运行，尊重 .gitignore 和大文件过滤，保护当前工作区。

**权限系统**：写文件、执行命令等操作展示彩色 diff 预览，用户可逐 hunk 选择性应用。

**Hooks**：通过 `.yunzhi/hooks.toml` 配置工具调用前后的自动化命令（如 pre-commit lint、post-edit format）。

## 配置与个性化

**用户配置**（`~/.yunzhi/config.toml`）：
```toml
api_key = "sk-xxxx"
model = "DeepSeek-V4-pro"
```

**Profile**（`.yunzhi/profiles.toml`）：按项目覆盖人格、模式、模型、token 上限和工具白名单：
```toml
[profiles.rust]
persona = "你是严格的 Rust 工程 reviewer"
mode = "agent"
model = "DeepSeek-V4-pro"
max_tokens = 4096
tools = ["read_file", "grep_search", "edit_file", "test_loop"]
```

**长期记忆**（`.yunzhi/memory.md`）：AI 可持久化存储项目约定和决策，条目化记忆只注入最近摘要避免上下文膨胀。

## 会话管理

交互模式支持完整的会话生命周期管理：
- `/session save <name>` - 保存当前会话
- `/session resume <name>` - 恢复会话
- `/session share <name>` - 导出分享
- `/session checkpoint <name> [note]` - 创建文件快照
- `/session rollback <name> <checkpoint>` - 回滚到快照点

## 许可证

本项目采用 **GNU Affero General Public License v3.0 (AGPL-3.0-only)**。这意味着你可以自由使用、修改和分发，但任何通过网络提供服务的修改版本也必须以相同许可证开源。

