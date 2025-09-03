# codex-core 架构概览

本文聚焦 `codex-rs/core`（crate 名：`codex-core`）的整体架构、对外接口、核心执行流与关键模块的职责边界，帮助你从“如何跑起来”与“代码分层”两个视角理解核心库。

---

## 1. 定位与职责

`codex-core` 是 Codex CLI 的“业务逻辑层/Agent 引擎”。它：
- 定义并实现与模型的回合式交互（请求/流式响应/工具调用/重试/错误处理）。
- 调度并封装“工具调用（shell、apply_patch、MCP 工具、plan 工具）”。
- 管理对话状态与历史、回合产物（如统一 diff）、审批安全策略与沙箱执行。
- 向上游（CLI/TUI/服务端）通过 `codex-protocol` 事件流报告过程状态与结果。

工作边界：
- 不直接关心终端交互和 UI 呈现（由 TUI/前端处理）。
- 通过 `codex-protocol` 复用跨语言的事件与消息模型；登录鉴权由 `codex-login` 提供；`apply_patch` 语义由 `codex-apply-patch` 提供；MCP 工具连接由 `codex-mcp-client` 提供。

主要依赖：
- 协议与类型：`codex-protocol`（事件、操作、配置枚举等统一定义，`lib.rs` 中 `pub use codex_protocol::protocol;` 直接重导出）。
- 模型/网络：`reqwest` + SSE 解析；Responses API 与 Chat Completions 双栈兼容。
- 执行/沙箱：macOS Seatbelt、Linux Landlock+seccomp（见 `seatbelt.rs`、`landlock.rs`、`exec.rs`、`spawn.rs`）。

---

## 2. 对外 API 与生命周期

核心对外类型：
- `Codex`（`src/codex.rs`）
  - `Codex::spawn(config, auth)`：初始化一个会话（Session），返回 `CodexSpawnOk { codex, session_id }`。
  - `codex.submit(op)` / `submit_with_id(sub)`：向会话发送操作（`Op`）。
  - `codex.next_event()`：按序拉取 `Event`（见 `codex-protocol`）。

Session 初始化（`Session::new`）并行完成：
- 回放/录制：`RolloutRecorder`（`rollout.rs`）用于会话回放与记录（可从 `experimental_resume` 恢复）。
- MCP 工具发现：`McpConnectionManager::new`（`mcp_connection_manager.rs`）同时启动多个 MCP 服务器并聚合工具列表。
- 默认 shell 探测：`shell::default_user_shell()`（识别 macOS zsh / Windows PowerShell）。
- 历史元数据：加载历史日志（数量、log_id）。

Session 持有：
- `tx_event`：对外事件通道；
- `state`：审批状态、当前任务、待注入输入、`ConversationHistory`；
- `mcp_connection_manager`：MCP 客户端集合；
- `notify`：回合结束后（Turn 完成）可触发外部通知程序；
- `user_shell`：默认用户 Shell；
- `show_raw_agent_reasoning`：是否透传“原始推理流”。

`submission_loop`（主事件循环）
- 持续接收 `Submission { id, op }`，根据 `op` 分派：
  - `Op::UserTurn` / `Op::UserInput`：开始或向当前任务注入新输入；
  - `Op::OverrideTurnContext`：按回合粒度覆盖 `model/effort/summary/cwd/approval/sandbox` 等；
  - `Op::ExecApproval` / `Op::PatchApproval`：处理用户审批（通过/拒绝/中止）；
  - `Op::ListMcpTools`、`Op::GetHistoryEntryRequest`、`Op::AddToHistory`、`Op::Compact` 等；
  - `Op::Shutdown`：结束会话并发送 `ShutdownComplete`。

---

## 3. 一次回合（Turn）的执行路径

高层流程（`run_task` → `run_turn`）：
1. 任务启动：记录用户输入为 `ResponseInputItem`，入历史。
2. 构建本回合的 `Prompt`（包含历史上下文、用户/系统指令、可用工具等）。
3. 通过 `ModelClient::stream` 建立与模型的流式连接：
   - Responses API：`client.rs::stream_responses`；
   - Chat Completions：`chat_completions.rs::stream_chat_completions` 并通过 `AggregatedChatStream` 聚合为“仅终态”行为，与 Responses API 对齐。
4. 消费流式事件（`ResponseEvent`）：
   - `OutputTextDelta` / 推理增量：透传为 `AgentMessageDelta` / `AgentReasoningDelta`（可选 `AgentReasoningRawContentDelta`）。
   - `OutputItemDone(item)`：
     - `Message{role=assistant}`：记录到历史并发送 `AgentMessage` 事件；
     - `FunctionCall` / `LocalShellCall`：进入工具调用分支（详见下节）。
   - `Completed{token_usage}`：结束本回合，推送 Token 用量；将本回合聚合的统一 diff（`TurnDiffTracker`）作为 `TurnDiffEvent` 发出。
5. Task 结束：发出 `TaskComplete` 并按策略（例如只保留最后一条 message）裁剪历史。

健壮性：
- SSE 掉线或“未到 Completed 就关闭”记为 `CodexErr::Stream` 并按 provider 的 `stream_max_retries` 退避重试。
- “工具调用未及时响应”场景会插入 synthetic 的 `FunctionCallOutput{success:false, content:"aborted"}` 保障调用闭合。

---

## 4. 工具系统与函数调用

工具描述与选择（openai_tools.rs）：
- `ToolsConfig` 决定暴露的工具集合：
  - Shell 工具会在以下三种之间按配置择一：默认 `shell`、`local_shell`（Chat Completions）、或“可流式 Shell”Function 工具对：`exec_command` + `write_stdin`。
  - 计划工具：`update_plan`。
  - 代码补丁：`apply_patch`（Freeform/Function 两种）。
  - Web 搜索：`web_search_preview`。
  - 查看本地图片：`view_image`（把本地图片文件加入上下文）。
- `create_tools_json_for_responses_api` / `create_tools_json_for_chat_completions_api`：同一套工具能力适配两种协议格式。
- MCP 工具输入 schema 的“类型修复”（`sanitize_json_schema`）以兼容部分服务端未显式声明 `type` 的情况，并确保工具顺序稳定以提升 Prompt 缓存命中率。

函数调用处理（`codex.rs::handle_function_call`）：
- `shell` / `container.exec`：
  - 解析 `ShellToolCallParams`（命令/超时/工作目录/可选权限提升理由）→ 转换为 `ExecParams`；
  - 交给 `handle_container_exec_with_params`，内部根据安全策略选用沙箱与审批流程（见第 5 节）。
- “可流式 Shell”（Responses API）：
  - `exec_command` 启动 PTY 会话，按 `yield_time_ms` 聚合一段时间内的 stdout/stderr 并返回，若进程仍在运行会在文本中标注 session_id；
  - `write_stdin` 向会话写入字符（支持控制字符，如 `\u0003` 代表 Ctrl-C），并在短时窗内收集后续输出；
  - 启用条件：`ToolsConfigParams.use_streamable_shell_tool = true` 时开放上述 Function 工具，否则回退到 `shell`/`local_shell`。
- `apply_patch`：
  - `ApplyPatchToolArgs` 载入补丁文本 → 交给 `apply_patch.rs::apply_patch` 做安全评估；
  - 安全通过会“委托给 exec”在沙箱中执行（或在显式批准下直接输出结果）；
  - 同时把补丁转换成 `protocol::FileChange`，用于 UI 展示与 `TurnDiffTracker` 前置基线。
- `update_plan`：
  - 解析 `UpdatePlanArgs` 并发出 `PlanUpdate` 事件；为模型提供结构化记录 TODO 的能力（不改变模型行为）。
- 未知函数名：返回结构化失败字符串，允许模型自我纠偏。

MCP 工具：
- 工具名采用“`<server>__<tool>`”全限定形式，来源于 `McpConnectionManager` 统一聚合与命名。
- `mcp_tool_call.rs::handle_mcp_tool_call`：发出 `McpToolCallBegin/End` 事件，串起执行与结果回填。

---

## 5. 命令执行、沙箱与安全审批

执行与流式输出（`exec.rs`）：
- `ExecParams`：命令、工作目录、超时、环境、是否请求“提升权限”、理由。
- `process_exec_tool_call`：根据 `SandboxType`（None / MacosSeatbelt / LinuxSeccomp）派发：
  - macOS：`seatbelt::spawn_command_under_seatbelt`，内嵌 SBPL 基线策略 + 按 `SandboxPolicy` 动态扩展可写路径与网络权限。
  - Linux：`landlock::spawn_command_under_linux_sandbox`，把 `SandboxPolicy` 与 cwd 序列化传递给 `codex-linux-sandbox` 助手执行。
  - 默认：`spawn_child_async` 直跑（用于已批准/已信任命令）。
- 流控与输出：
  - 统一限制最多 10KiB/256 行，边读边经 `ExecCommandOutputDelta` 推流；
  - 超时、Ctrl-C、信号等场景进行规范化退出码与错误映射。

可流式 Shell 会话（`exec_command/`）：
- `SessionManager` 管理带 PTY 的会话、stdin 写入与多消费者增量输出广播；
- `exec_command` 参数：`cmd`、`yield_time_ms`、`max_output_tokens`、`shell`、`login`；
- `write_stdin` 参数：`session_id`、`chars`、`yield_time_ms`、`max_output_tokens`；
- 输出采用“中部截断”策略（优先选取换行边界），在正文中插入 `…N tokens truncated…` 标记；
- 应用补丁期间为避免噪音，常规 Exec 增量会被抑制（见 `codex.rs` 中的条件判断）。

安全策略（`safety.rs` + `is_safe_command.rs`）：
- `AskForApproval`：Never / OnFailure / OnRequest / UnlessTrusted；
- `SandboxPolicy`：ReadOnly / WorkspaceWrite{writable_roots, network_access...} / DangerFullAccess；
- `assess_command_safety`：
  - 已知安全命令（内置白名单 + 简化 Bash 语法树判定）直接放行；
  - 未信任命令依据 `approval_policy + sandbox_policy + with_escalated_permissions` 决定：自动批准（选择可用沙箱）/ 请求用户批准 / 拒绝。
- `apply_patch` 安全：
  - `assess_patch_safety` 判断补丁是否完全落在可写根内；若是且平台支持沙箱，则可自动通过并强制在沙箱中执行；否则依据策略请求用户或拒绝。

环境变量注入（`exec_env.rs`）：
- `ShellEnvironmentPolicy` 提供继承/排除/仅包含/覆盖等构造规则，保障最小泄露原则；
- `spawn.rs` 会根据 `SandboxPolicy` 设置一些运行时标识环境变量（例如网络受限标志、沙箱标志），供子进程与测试探测环境。

---

## 6. 对话、历史与上下文注入

对话历史（`conversation_history.rs`）：
- 合并相邻的 assistant 消息，避免“流式增量 + 终态重复”；
- `keep_last_messages(n)` 保留最近 N 条用户/助手消息；
- 历史持久化读写在 `message_history.rs`（通过 `Op::AddToHistory`/`Op::GetHistoryEntryRequest`）。

上下文注入：
- 项目文档：`project_doc.rs` 读取（含大小上限），拼入系统/用户指令；
- 环境上下文：`environment_context.rs` 将 cwd、审批策略、沙箱模式、网络可达性、默认 shell 序列化为简单 XML，作为“用户消息”注入给模型，帮助其自适应运行环境。

---

## 7. 统一差异跟踪（TurnDiffTracker）

`turn_diff_tracker.rs`：
- 在“回合周期”维度聚合对文件的新增/删除/改动/重命名，统一输出 Git 风格的 `unified diff`，在 `response.completed` 时机通过 `TurnDiffEvent` 发出；
- 通过 in‑memory 基线快照 + 当前磁盘状态对比构造差异，尽可能做到平台无关、路径稳定与一致排序；
- 对二进制/符号链接/执行位变化等情况做专门处理。

---

## 8. 模型层：Responses vs Chat Completions

`client.rs` / `chat_completions.rs`：
- Provider 信息：`model_provider_info.rs` 描述 API 端点、鉴权、重试/超时等；内置 openai 与 `oss`（默认 11434 端口）两类；
- Responses API：
  - request：`ResponsesApiRequest`，包含 instructions、input、tools、reasoning（effort/summary）、store、stream、include；
  - headers：`originator`、`User-Agent`（`user_agent.rs` 生成）、`OpenAI-Beta: responses=experimental`、`session_id` 等；
  - 流解析：处理 `response.delta.*`、`response.output_item.done`、`response.completed` 等，必要时解析 `function_call.arguments` 的分片聚合；
  - 错误与退避：HTTP 非 2xx、`rate_limit_exceeded`、`Retry-After`、SSE idle 超时等。
- Chat Completions：
  - 将历史转换为 `messages`，工具转换为 `tools`（function 形式）；
  - 通过 `AggregatedChatStream` 折叠 token‑level delta，仅向上游暴露“完成的 assistant 消息 + Completed”。

---

## 9. 通知与可观测性

- 用户通知：`user_notification.rs` 序列化通知 JSON 作为外部命令参数（由 `Config.notify` 配置），例如“回合完成”。
- 事件对齐：除文本/推理增量外，还包含 `TokenCount`、`PlanUpdate`、`ApplyPatchApprovalRequest`、`ExecApprovalRequest`、`McpToolCall*`、`TurnDiff`、`TaskStarted/Complete` 等，方便前端或日志系统消费。
- 错误映射：`error.rs` 将底层错误映射到用户可理解的消息；`get_error_message_ui` 对沙箱拒绝等进行友好展示。

---

## 10. 与其它 crate 的关系

- `codex-apply-patch`：apply_patch 解析/合法性校验/统一差异结构；
- `codex-login`：API Key/ChatGPT OAuth 等鉴权封装；
- `codex-mcp-client`、`mcp-types`：MCP 连接与工具 schema；
- `codex-protocol`：跨进程/跨语言的事件与数据模型统一源；
- 平台沙箱：Linux 依赖外部 `codex-linux-sandbox` 可执行文件（`Config.codex_linux_sandbox_exe` 提供路径），macOS 借助系统 Seatbelt。

---

## 11. 关键文件速览

- 入口与主循环：`src/lib.rs`、`src/codex.rs`
- 模型客户端：`src/client.rs`、`src/chat_completions.rs`、`src/client_common.rs`
- 工具与函数调用：`src/openai_tools.rs`、`src/plan_tool.rs`、`src/mcp_connection_manager.rs`、`src/mcp_tool_call.rs`、`src/apply_patch.rs`
- 执行与沙箱：`src/exec.rs`、`src/seatbelt.rs`、`src/landlock.rs`、`src/spawn.rs`、`src/shell.rs`、`src/exec_env.rs`
- 安全与审批：`src/safety.rs`、`src/is_safe_command.rs`
- 上下文与历史：`src/environment_context.rs`、`src/conversation_history.rs`、`src/message_history.rs`、`src/project_doc.rs`
- 其他：`src/model_provider_info.rs`、`src/models.rs`、`src/parse_command.rs`、`src/turn_diff_tracker.rs`、`src/user_agent.rs`、`src/error.rs`、`src/git_info.rs`

---

## 12. 设计要点回顾

- 对外暴露“提交操作/消费事件”的最小接口，内部通过 `submission_loop` 协调任务与回合。
- Responses 与 Chat Completions 双栈适配，保证上游看到一致的事件序列与工具体验。
- 工具调用统一经由安全评估 → 审批 → 沙箱执行的管道，最小权限、默认安全。
- 基于回合聚合的统一 diff，提供对用户“本回合做了什么”的清晰反馈。
- MCP 工具兼容：自动聚合命名、输入 schema 清洗、调用事件透明化。


# 代码阅读建议
如果你想进一步深入：建议从 `src/codex.rs` 的 `Codex::spawn`、`submission_loop`、`run_task`、`handle_function_call` 四个入口向外扩展阅读；遇到跨模块接口，直接跳至上文“关键文件速览”的相应文件对照理解。


整体入口

- lib.rs: 认识模块地图与重导出
    - 路径: core/src/lib.rs
    - 看点: 模块划分、对 codex-protocol 的重导出（protocol）、常用工具与常量导出
- codex.rs: 会话/事件总线与主循环
    - 路径: core/src/codex.rs
    - 看点: Codex::spawn、Codex 结构、提交通道/事件通道、ConfigureSession/Session 初始化、事件派发与工具调用入口

配置与协议上下文

- 配置族: 读完再回到 codex.rs 更清晰
    - config.rs/config_types.rs/config_profile.rs/flags.rs/environment_context.rs
    - 看点: 模型提供方/推理级别、批准/沙箱策略、工作目录、指令注入、环境变量
- 协议定义（被 core 重导出）
    - 路径: protocol/src/*.rs（只扫类型：Event、Submission、工具相关事件/枚举）
    - 看点: 事件流的外观与边界契约，帮助理解 codex.rs 的输入输出

模型与工具（从高到低）

- 模型请求链
    - client_common.rs/client.rs: Prompt/ResponseEvent、ModelClient 实现与调用
    - model_family.rs/model_provider_info.rs/openai_model_info.rs: 模型元信息和分流
    - openai_tools.rs: 将“工具”挂到模型（OpenAI 工具调用参数构建、工具配置）
    - project_doc.rs: 会话前置用户指令注入
- 工具路由与命令解析
    - parse_command.rs: 用户/模型指令解析（slash/工具调用等）
    - plan_tool.rs: 计划工具（update_plan）处理

执行与沙箱（两条主线）

- 非交互执行链（一次性执行）
    - exec.rs: process_exec_tool_call 及 ExecParams/ExecToolCallOutput 主路径
    - exec_env.rs/shell.rs/spawn.rs: 环境构建、命令拼装、进程启动与 IO
    - safety.rs/is_safe_command.rs: 风险评估与白/黑名单
    - seatbelt.rs/landlock.rs: macOS Seatbelt 与 Linux Landlock+seccomp 的具体封装
- 流式会话执行（PTY + stdin 写入）
    - exec_command.rs: ExecSessionManager、ExecCommandParams、WriteStdinParams、工具名常量
    - 看点: PTY 会话生命周期、输出广播、stdin 写入、退出码传递

补丁与 MCP（常见关键路径）

- 补丁应用
    - apply_patch.rs/tool_apply_patch.rs: 与 codex-apply-patch 的桥接、审批事件、执行/回传
- MCP 调用
    - mcp_connection_manager.rs/mcp_tool_call.rs: MCP 会话与工具调用转发

会话状态与追踪

- conversation_manager.rs/conversation_history.rs/message_history.rs: 会话与消息历史管理
- turn_diff_tracker.rs: 每次回合的工作区改动追踪（配合 apply-patch/渲染）
- rollout.rs/git_info.rs/user_agent.rs: 版本/回滚记录、Git 信息、UA
- error.rs/user_notification.rs/terminal.rs/util.rs: 错误分层、用户通知、终端能力、通用工具

推荐阅读路径（次序与“穿线”）

1. lib.rs → codex.rs（主控/队列/事件）
2. config.* → protocol（只扫类型）→ 回看 codex.rs 的事件分发
3. client_common.rs → client.rs → openai_model_info.rs/model_family.rs → openai_tools.rs
4. parse_command.rs → plan_tool.rs（小结工具如何进入主循环）
5. exec.rs → exec_env.rs/shell.rs/spawn.rs → safety.rs/is_safe_command.rs → seatbelt.rs/landlock.rs
6. exec_command.rs（流式会话/stdin 写入）
7. apply_patch.rs/tool_apply_patch.rs → mcp_connection_manager.rs/mcp_tool_call.rs
8. conversation_manager.rs/conversation_history.rs/message_history.rs → turn_diff_tracker.rs → 其他辅助模块

阅读技巧

- 先“宽读函数签名+注释”，再按一次“完整任务流”深追：提交 → 模型 → 工具 → 执行/流式 → 事件回传 → 补丁/历史记录。
- 在 codex.rs 搜索 “Tool”/“ExecCommand”/“ApplyPatch”/“Mcp” 关键字，顺藤摸瓜到对应模块。
- 对 PTY/沙箱等复杂点，先看公共类型与接口，再看具体平台实现。

需要的话，我可以按这个顺序带你逐段过代码，并画一张“提交/事件/工具/执行”的数据流图作为备忘。