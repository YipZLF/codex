# Codex 仓库架构综述（自顶向下）

本文面向需要快速理解 Codex CLI（本地智能编码代理）代码实现的工程师，按自顶向下的方式梳理整体架构、模块边界与关键细节。重点聚焦 Rust 版 CLI（`codex-rs` 工作区），同时简述周边工具与分发包装。

---

## 1. 顶层视图

- 目标：在本地以高可靠、可控的方式运行智能编码代理，支持命令执行、补丁应用、TUI 交互、非交互脚本模式，以及沙箱与审批策略。
- 主要组成：
  - Rust 工作区 `codex-rs` 提供核心逻辑（`codex-core`）、TUI（`codex-tui`）、非交互 CLI（`codex-exec`）、多工具入口（`codex-cli`）、协议与 MCP 集成等若干 crate。
  - Node 包 `codex-cli`（根目录同名文件夹）作为分发包装：选择并拉起对应平台的原生二进制。
- 关键设计：
  - 明确的“提交/事件”（Submission/Event）协议，将前端（TUI/CLI）与核心业务逻辑解耦。
  - 工具化能力（shell、apply_patch、plan 更新等）通过统一的工具描述注入模型调用流。
  - 多平台沙箱（macOS Seatbelt、Linux Landlock+seccomp）+ 执行审批策略，守护安全。

---

## 2. 代码结构总览（仓库根）

- `codex-rs/`：Rust Cargo workspace（核心）。
- `codex-cli/`：Node 分发包装（选择 `bin/codex-<target>` 并转发信号、PATH 等）。
- `docs/`：文档与发布流程说明。
- 顶层 `README.md`：用户向的功能、安装、登录、沙箱策略等说明。

---

## 3. Rust 工作区（`codex-rs`）

工作区成员（部分）：

- 核心与公共
  - `core`（crate: `codex-core`）：业务核心，模型调用、会话/事件流、工具处理、命令执行与沙箱、配置、历史、补丁应用集成等。
  - `common`（crate: `codex-common`）：CLI 共享参数解析、配置摘要、模糊匹配、预设等通用组件。
  - `protocol`（crate: `codex-protocol`）：跨进程/前后端共享的“提交/事件”协议与类型（包括计划工具、命令解析等）。
  - `protocol-ts`（crate: `codex-protocol-ts`）：从 Rust 类型生成 TypeScript 绑定（便于前端/扩展集成）。
- 终端 UI 与 CLI
  - `tui`（crate: `codex-tui`）：Ratatui 全屏终端 UI；快照测试较多（`insta`）。
  - `exec`（crate: `codex-exec`）：非交互模式（批处理/CI），直接在终端输出人类可读或 JSON。
  - `cli`（crate: `codex-cli`）：多工具入口（二进制名 `codex`），将子命令路由到 TUI/Exec/MCP 等。
- 沙箱与安全
  - `linux-sandbox`（crate: `codex-linux-sandbox`）：Linux 端封装，落地 Landlock + seccomp。
  - `execpolicy`（crate: `codex-execpolicy`）：策略解析与命令检查（Starlark 规则、正负示例等）。
- 工具与集成
  - `apply-patch`（crate: `codex-apply-patch`）：补丁语法解析与文件系统应用、影响路径统计；也提供“是否为 apply_patch 调用”的识别与正确性校验。
  - `file-search`（crate: `codex-file-search`）：基于 `ignore` + `nucleo-matcher` 的快速文件名模糊搜索，TUI 的 `@` 搜索即基于此。
  - `mcp-types`、`mcp-client`、`mcp-server`：Model Context Protocol 类型与简化客户端/服务器框架。
  - `ollama`（crate: `codex-ollama`）：开源模型（OSS）支持，校验/拉取本地模型，默认 `gpt-oss:20b`。
  - `ansi-escape`（crate: `codex-ansi-escape`）：将 ANSI 转为 Ratatui 文本/行，便于彩色输出渲染。
  - `arg0`（crate: `codex-arg0`）：二进制多入口（arg0 trick）：`codex`/`codex-linux-sandbox`/`apply_patch` 路由与 `.env` 安全注入。
  - `chatgpt`（crate: `codex-chatgpt`）：与 ChatGPT 登录联动的工具（如 `codex apply`）。
  - `login`（crate: `codex-login`）：鉴权（`auth.json`、API Key、ChatGPT Token 刷新与存储）。
  - `protocol-ts`：从 `codex-protocol` 导出 TS 类型（配合 MCP/前端等）。

> Crate 命名约定：目录名如 `core`，crate 名以 `codex-` 前缀（如 `codex-core`）。

---

## 4. 核心库 `codex-core`

### 4.1 会话与事件流

- 核心入口 `codex.rs` 暴露 `Codex`/`Session` 与 `submit()` 接口，封装“提交队列 SQ / 事件队列 EQ”模型：
  - 提交（`protocol::Op`）：如 `UserTurn`、`UserInput`、`ExecApproval`、`PatchApproval`、`OverrideTurnContext` 等。
  - 事件（`protocol::EventMsg`）：如 `AgentMessage*`、`Exec*`、`ApplyPatch*`、`PlanUpdate`、`TurnDiff`、`TaskComplete`、`Error` 等。
- `ConversationManager` 管理长会话与初始化（`SessionConfigured`），`message_history`/`conversation_history` 负责历史存储与截断策略。

### 4.2 模型调用（Responses/Chat 两栈）

- `client.rs` 统一封装模型请求：根据 `ModelProviderInfo.wire_api` 选择 Responses 或 ChatCompletions 流式实现。
  - Responses：构造 `ResponsesApiRequest`，可控制 `reasoning`（力度/摘要）、`store`（配合 ZDR）与 `include` 字段；聚合流式事件为最终助手消息与 Token 计数（`TokenUsage`）。
  - ChatCompletions：使用适配器将增量流聚合或直出 Raw Reasoning 内容（受 `show_raw_agent_reasoning` 控制）。
- 工具注入：`openai_tools.rs` 根据配置/模型族生成工具 JSON（Responses 与 Chat 形态差异）。内置：
  - `shell`：本地命令执行（可带 `with_escalated_permissions`/`justification`，文案会解释何时要申请升级权限）。
  - `apply_patch`：文件编辑的结构化工具（统一补丁语法，详见 4.5）。
  - `update_plan`：计划更新工具（详见 4.6）。
  - MCP 工具：将外部 MCP 服务器工具映射为 OpenAI 工具（`mcp_tool_to_openai_tool`）。

### 4.3 命令执行与沙箱

- 抽象：`exec.rs` 负责执行工具调用（`process_exec_tool_call`），输出增量流（`ExecCommandOutputDeltaEvent`）与结束事件；限制最大输出（字节/行）。
- 沙箱后端：
  - macOS：`seatbelt.rs` 生成 `sandbox-exec` 策略（只信任 `/usr/bin/sandbox-exec`），以参数传递可写根目录并排除只读子路径（如顶层 `.git`）。
  - Linux：`landlock.rs` + `codex-linux-sandbox` 子进程，通过 seccomp/landlock 组合限制；`spawn` 统一抽象不同平台的进程拉起与环境注入。
- 安全/审批：
  - `safety.rs`、`is_safe_command.rs`：静态启发式 + 策略判断是否“已知安全”，结合 `AskForApproval` 策略（`on-request`/`on-failure`/`never` 等）。
  - `execpolicy` crate：Starlark 语法的策略引擎（`default.policy`），对命令/参数进行更细粒度匹配、正负样例校验，作为“安全建议/约束”的辅助判定。

### 4.4 配置系统

- 结构体 `config::Config` 聚合：模型选择/族、上下文窗口、审批与沙箱策略、TUI 设置、通知脚本、历史、MCP 服务器、文件打开器 URI、ZDR/Reasoning 控制、`codex_home`、`cwd` 等。
- 载入流程：
  - 基于 `~/.codex/config.toml`（可由 `CODEX_HOME` 覆盖）→ 应用 `-c key=value` CLI 临时覆盖 → 再应用强类型 `ConfigOverrides`（如 `--sandbox`/`--ask-for-approval`/`--oss`/`--cd` 等）。
  - 支持基于 `AGENTS.md`/项目文档（截断到 `PROJECT_DOC_MAX_BYTES`）注入额外用户指令。
  - 支持 `notify`：每回合结束向外部脚本发 JSON 负载（用于系统通知）。

### 4.5 文件编辑：`apply_patch`

- 统一补丁语法（易解析、安全的文件级 diff envelope）：支持 Add/Update/Delete、Move、hunk、`*** End of File` 截断标记。
- `codex-apply-patch` 提供：
  - 解析与错误报告（细化到 hunk 行号/原因）。
  - 影响路径统计（新增/修改/删除），以及将“补丁工具调用”投影为具体文件系统操作（创建父目录、写入/删除、移动）。
  - 从 shell 形态（heredoc）提取真实补丁体并校验。

### 4.6 计划工具：`update_plan`

- 以函数调用的方式让模型结构化输出自己的“计划与步骤状态”，事件以 `PlanUpdate(UpdatePlanArgs)` 发送给前端（TUI/CLI）渲染。
- 约束：同一时刻最多一个 `in_progress`，其余为 `pending`/`completed`。

### 4.7 其它关键模块

- `parse_command.rs`：将任意 shell 命令解析为语义类别（读文件/搜索/测试/格式化/未知…），用于对用户展示“它正在做什么”。
- `git_info.rs`、`turn_diff_tracker.rs`：展示与追踪当前回合的文件差异/状态（TUI 右侧面板等）。
- `project_doc.rs`：从项目根拉取 `AGENTS.md`、环境上下文组装“用户指令”。
- `mcp_connection_manager.rs`/`mcp_tool_call.rs`：与 MCP 客户端工具调用的桥接。

---

## 5. TUI（`codex-tui`）

- 启动与 CLI：`run_main()` 将 CLI 参数解析为 `ConfigOverrides`，可快捷设置 `--sandbox`、`--cd`、`--oss` 等。自动写日志（`tracing_subscriber`），显示配置摘要与权限级别。
- 架构：
  - `app.rs` 顶层状态机：Onboarding/Chat 两大视图；键盘/粘贴/窗口大小事件从 crossterm 读出，经 `AppEventSender` 分发，UI 渲染走统一的调度与去抖（`REDRAW_DEBOUNCE`）。
  - `chatwidget/`：核心聊天与消息流组件，逐事件渲染模型消息 / 原因链 / 函数调用 / 执行输出。
  - `file_search/`：`@` 触发模糊搜索，调用 `codex-file-search`。
  - `user_approval_widget/`：审批对话框（命令/补丁）。
  - 主题与样式：参见 `tui/styles.md`；使用 Ratatui `Stylize` 简洁表达（如 `"text".dim()`、`"M".red()`），避免直接手写 `Style`。
- 测试：大量基于 `insta` 的快照测试（渲染输出），有 `vt100` 重放测试模式；UI 变更需按仓库说明更新快照。

---

## 6. 非交互 CLI：`codex-exec`

- 用途：CI/自动化/脚本模式；命令 `codex exec` 或直接运行 `codex-exec`。
- 行为：
  - 解析提示词（位置参数或 stdin），按 `--json` 决定输出格式；根据 `--oss` 触发本地 OSS 模型准备（`codex-ollama`）。
  - 打印配置摘要与提示词；可选 Git 仓库检查（默认要求在受信任目录内运行）。
  - 通过 `ConversationManager` 与核心进行一次或多次“提交→事件”交互，直至 `TaskComplete`。

---

## 7. 多工具入口：`codex-cli`

- 二进制 `codex` 的路由：
  - 默认进入 TUI（无子命令）。
  - 子命令：`exec`、`login`/`logout`、`mcp`（以 MCP 服务器运行）、`proto`（协议 stdin/stdout 模式）、`completion`（生成补全脚本）、`debug seatbelt/landlock`（沙箱实验）、`apply`（应用最近一次补丁）、`generate-ts`（导出 TS 类型）。
- `arg0` trick：同一可执行在 Linux 下也可作为 `codex-linux-sandbox` 入口使用；另内置对 `apply_patch` 的直达处理（便于外部复用）。

---

## 8. 沙箱与策略细节

- macOS（Seatbelt）：
  - 基础策略 `seatbelt_base_policy.sbpl` + 按需拼接“只读/可写根目录/网络”策略片段，`-DWRITABLE_ROOT_*` 注入绝对路径与只读子路径（例如排除 `.git`）。
  - 仅信任系统 `/usr/bin/sandbox-exec`。
- Linux（Landlock + seccomp）：
  - 独立二进制 `codex-linux-sandbox` 负责以最小权限启动命令，结合 `spawn`/`exec` 统一控制 I/O、超时、信号退出码（如超时 `64`）。
- 执行策略（`codex-execpolicy`）：
  - Starlark 表达策略，包含参数匹配/白名单/禁止项、正负样例检查，辅助 `is_safe_command` 与审批策略形成“默认自动/请求审批/失败再升级”的组合。

---

## 9. 鉴权与登录（`codex-login`）

- 形态：
  - API Key：读取 `OPENAI_API_KEY` 或 `auth.json` 中的 `OPENAI_API_KEY` 字段。
  - ChatGPT：本地起服务完成 OAuth，`auth.json` 存储 `id_token`/`access_token`/`refresh_token`/`last_refresh`；自动在 28 天后刷新。
- 选择：`preferred_auth_method` 可强制优先级（默认倾向 ChatGPT，否则回退 API Key）。

---

## 10. 模型与 OSS（`codex-ollama`）

- `--oss` 模式：检查本地 Ollama 服务可用；若默认模型（`gpt-oss:20b` 或用户指定）未就绪则拉取；失败非致命，后续请求可再反馈。

---

## 11. 协议与跨语言

- `codex-protocol`：
  - `Op`（提交）与 `EventMsg`（事件）枚举，配套数据结构（`TokenUsage`、`Exec*`、`ApplyPatch*`、`PlanUpdate`、`TurnDiff` 等）。
  - `SandboxPolicy`：`read-only`/`workspace-write{writable_roots,network_access,...}`/`danger-full-access`，并提供 `get_writable_roots_with_cwd()` 等辅助。
- `mcp-types`：由 `schema/` 生成的 MCP 请求/响应/通知类型（`generate_mcp_types.py`），为 `mcp-client`/`mcp-server` 与外部生态共享。
- `protocol-ts`：将 Rust 类型导出为 TS 文件供前端/扩展使用。
- Node 分发（`codex-cli/bin/codex.js`）：解析平台三元组，spawn 对应原生二进制；转发信号，拼接 PATH（如 VSCode ripgrep）。

---

## 12. 构建、风格与测试

- 工作区设置：统一 `edition = 2024`、Clippy 严格规则（禁止 `unwrap/expect` 在核心路径等）。
- TUI 风格：
  - 文本样式使用 Ratatui `Stylize`（`"text".dim()/red()/green()/magenta()`），遵循 `tui/styles.md` 色彩约定，避免自定义颜色导致对比度问题。
- 快照测试：
  - `codex-tui` 使用 `insta`；UI 变更后需跑 `cargo test -p codex-tui` 并按需 `cargo insta accept -p codex-tui`。
- 常见开发命令（在 `codex-rs` 目录）：
  - 格式化：`just fmt`
  - Lint 修复：`just fix`
  - 运行子项目测试：`cargo test -p <crate>`；公共/核心/协议改动后建议 `cargo test --all-features`。

> 注意：不要修改任何与 `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` 或 `CODEX_SANDBOX_ENV_VAR` 相关的代码。

---

## 13. 典型数据流（从用户到结果）

1) 用户在 TUI/Exec 输入需求与（可选）图片 → 组装 `Prompt`（含工具、指令拼接与 `AGENTS.md`）。
2) Core 依据模型族与配置选择 Responses/Chat，注入工具 schema 并发起流式请求。
3) 模型可能调用 `shell`/`apply_patch`/`update_plan`/MCP 工具：
   - `shell` 执行进入沙箱，按策略决定是否自动执行或发起审批（`ExecApprovalRequest`）。
   - `apply_patch` 解析补丁，文件系统变更并发 `PatchApplyBegin/End` 与差异事件。
   - `update_plan` 产生计划变更事件，前端更新“进行中/已完成”。
4) Core 将消息、token 计数、执行输出增量、审批请求等统一封装为 `EventMsg` 流返回前端。
5) 前端渲染消息/引用/差异/图片等，若触发审批则展示交互控件，最终以 `TaskComplete` 结束本回合。

---

## 14. 扩展点与二次开发建议

- 新工具：按 `openai_tools.rs` 规范定义 JSON-Schema，结合后端处理逻辑（函数参数解析/事件上报）。
- 新模型提供方：扩展 `model_provider_info.rs` 与配置；确保 `wire_api`/鉴权/端点一致。
- UI 能力：在 TUI 的 `chatwidget`/`user_approval_widget`/`status_indicator_widget` 等处扩展渲染与交互；保持 `Stylize` 风格一致。
- 策略定制：依据团队合规需求定制 `execpolicy`（Starlark），与 `SandboxPolicy` 组合实现差异化权限。

---

如需更细节的某一模块源码导读（例如 `codex-core` 的事件循环、TUI 的渲染与状态管理、`apply_patch` 的 hunk 应用算法、或 MCP 交互协议），可告知我进一步展开。

