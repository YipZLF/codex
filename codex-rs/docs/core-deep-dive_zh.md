# Codex Core 关键实现逐段导读

> 本文按《core-reading-guide_zh.md》的顺序，带你逐段过 `codex-core` 的关键实现与调用链。每节包含：核心职责、关键类型/函数、读代码时的关注点与顺藤摸瓜的下一跳。

---

## 1) 入口与总线：lib.rs → codex.rs

- 核心职责
  - `lib.rs`：导出模块地图与别名，重导 `codex-protocol` 为 `codex_core::protocol::*`；禁止库代码直接 `print!` 到终端。
  - `codex.rs`：会话生命周期与事件总线；任务调度；模型回合主循环；工具调用路由；审批与补丁事件；exec 输出格式化等。

- 关键类型
  - `Codex`：维护提交通道 `tx_sub` 与事件通道 `rx_event`。
    - `spawn(config, auth_manager, initial_history)` → `CodexSpawnOk { codex, session_id }`
    - `submit(op) -> sub_id`、`next_event()`
  - `Session`：单实例状态与外部交互桥。
    - 字段（择要）：
      - `session_id: Uuid`
      - `tx_event: Sender<Event>` 事件回传
      - `mcp_connection_manager`（MCP 工具）
      - `session_manager: ExecSessionManager`（流式 PTY 会话）
      - `state: Mutex<State>`（审批/历史/待注入输入/当前任务）
      - `codex_linux_sandbox_exe`、`user_shell`、`notify`
  - `TurnContext`：单回合上下文（模型/审批/沙箱/CWD/工具配置）。
  - `State`：
    - `approved_commands: HashSet<Vec<String>>`
    - `pending_approvals: HashMap<String, oneshot::Sender<ReviewDecision>>`
    - `pending_input: Vec<ResponseInputItem>`
    - `history: ConversationHistory`

- 初始化流程（`Codex::spawn` 内部）
  - 组合 `ConfigureSession`（provider/model/推理/审批/沙箱/通知/CWD 等）。
  - `Session::new`：构建 `Session` 与 `TurnContext`；记录初始会话项：
    - 用户指令：`Prompt::format_user_instructions_message(..)`
    - 环境：`EnvironmentContext::new(cwd, approval_policy, sandbox_policy, user_shell)`
  - 发送 `SessionConfiguredEvent { session_id, model, history_log_id, history_entry_count }`（以及配置错误等事件）。

- 常用方法（节选）
  - 审批：
    - `request_command_approval(..) -> oneshot::Receiver<ReviewDecision>` → 发送 `ExecApprovalRequestEvent`
    - `request_patch_approval(..)` → 发送 `ApplyPatchApprovalRequestEvent`（包含 `convert_apply_patch_to_protocol` 后的路径变更）
    - `notify_approval(sub_id, decision)`：完成审批
  - 任务：`set_task(task)`、`remove_task(sub_id)`、`interrupt_task()`（清空挂起并发出 `TurnAborted`）
  - 输入注入与历史：`inject_input(items)`、`get_pending_input()`、`record_conversation_items(items)`
  - 事件发送：`send_event(event)`（错误吞掉并记录）
  - 通知：`maybe_notify(UserNotification)`（fire-and-forget 外部进程）

- 执行前后事件（显示/审计）
  - `on_exec_command_begin(..)`
    - 普通命令 → `ExecCommandBegin { command, cwd, parsed_cmd }`
    - 补丁 → `PatchApplyBegin { auto_approved, changes }`，并 `turn_diff_tracker.on_patch_begin`
  - `on_exec_command_end(..)`
    - 汇总 `stdout/stderr` 全量文本 + `aggregated_output`；
    - 构造“供模型阅读”的字符串：`format_exec_output_str(..)`（见下）

- Exec 输出格式化（模型输入专用）
  - `format_exec_output_str(exec_output)`：
    - 约束：行数上限 `MODEL_FORMAT_MAX_LINES=256`、字节预算 `MODEL_FORMAT_MAX_BYTES=10KiB`；
    - 策略：取 Head+Tail，中间插入 `[.. omitted N of TOTAL lines ..]` 标记；
    - 字节预算不足时在字符边界截取（`take_bytes_at_char_boundary`/`take_last_bytes_at_char_boundary`）。

- 顺藤摸瓜
  - 提交/调度：`submission_loop(..)` 与 `AgentTask::spawn(..)`/`run_task(..)`
  - 工具处理：`handle_function_call(..)`、`handle_custom_tool_call(..)`、`handle_container_exec_with_params(..)`

---

## 2) 配置与协议上下文

- 配置族（`config.rs/config_types.rs/config_profile.rs/flags.rs/environment_context.rs`）
  - 模型提供方/族、推理力度/摘要、审批策略（`AskForApproval`）、沙箱策略（`SandboxPolicy`）、Shell 环境策略（profile/PowerShell）、工作目录、禁用远端存储标志等。
- 协议（`codex-protocol`，由 core 重导）
  - 关注：`Event/EventMsg/Submission/ResponseItem`、工具事件（审批/执行开始结束/输出增量等）、`ParseCommand` 映射、`WebSearch*`、`TokenUsage` 等。

---

## 3) 模型与工具挂载：client_common.rs → client.rs → model_* → openai_tools.rs

- Prompt/流（`client_common.rs`）
  - `Prompt { input, store, tools, base_instructions_override }`
  - `get_full_instructions(..)`：BASE_INSTRUCTIONS + 按模型族决定是否附加 `apply_patch` 的使用说明。
  - 事件：`ResponseEvent::{Created, OutputItemDone(..), Completed{..}, OutputTextDelta, Reasoning*Delta, WebSearchCallBegin}`
  - 可选 verbosity（GPT‑5）与 reasoning（summary/effort）。

- 模型客户端（`client.rs`）
  - `ModelClient::stream(..)`：按 provider 的 `wire_api` 分派到 Responses 或 Chat；
  - Responses：构造 `ResponsesApiRequest`（含 tools JSON、reasoning、store、include、prompt_cache_key=session_id）；SSE 流转 `ResponseStream`。
  - Chat：先拿原始事件流，再用 `AggregateStreamExt` 聚合以对齐 Responses 的“每回合仅一个最终 assistant 消息”。

- 工具构建（`openai_tools.rs`）
  - `ToolsConfig::new(..)`：决定 shell 形态（默认/带审批/Local/Streamable）、是否挂载 `plan`、`apply_patch`（freeform/function）、`web_search`、`view_image`。
  - `get_openai_tools(config, mcp_tools)`：汇总工具，MCP 工具按“全名”排序保证提示缓存命中；Streamable 会附加 `exec_command`/`write_stdin`。

---

## 4) 指令解析与计划：parse_command.rs、plan_tool.rs

- `parse_command.rs`
  - 将任意命令解析为 `ParsedCommand`（Read/ListFiles/Search/Format/Test/Lint/Noop/Unknown），供 `ExecCommandBeginEvent.parsed_cmd` 呈现；
  - 提示：该文件上方自带大量测试样例，利于 TDD 修改解析行为。
- `plan_tool.rs`
  - 处理 `update_plan` 工具调用，TUI 侧会订阅与渲染。

---

## 5) 一次性执行链（非流式）：exec.rs → exec_env.rs/shell.rs/spawn.rs → safety.rs/is_safe_command.rs → seatbelt.rs/landlock.rs

- 主入口（`exec.rs::process_exec_tool_call(..)`）
  - 选择沙箱：`SandboxType::{None, MacosSeatbelt, LinuxSeccomp}`；
  - `spawn_command_under_*` 或 `spawn_child_async` 产生 `Child`，`consume_truncated_output(..)` 读取 stdout/stderr，并通过 `StdoutStream` 限额推送增量事件 `ExecCommandOutputDelta`（最多 `MAX_EXEC_OUTPUT_DELTAS_PER_CALL`）。
  - 超时：统一杀进程并返回合成的 `ExitStatus`（TIMEOUT/信号）。
  - 错误映射：对“疑似沙箱拒绝”保守处理；非 0 退出码 + 平台判断 → `SandboxErr::Denied/Timeout/Signal`。
  - 返回 `ExecToolCallOutput { exit_code, stdout, stderr, aggregated_output, duration }`。

- 环境与进程（`exec_env.rs/shell.rs/spawn.rs`）
  - 构建执行环境（PATH、profile 等）、拼装命令、I/O 策略（工具事件来源）。

- 安全与沙箱（`safety.rs/is_safe_command.rs` + `seatbelt.rs/landlock.rs`）
  - 评估是否需要审批/提权；平台沙箱封装与错误映射，Linux 需要 `codex-linux-sandbox` 可执行配合。

---

## 6) 流式会话（PTY + stdin）：exec_command/*

- 工具定义与导出（`exec_command/mod.rs`）
  - 类型：`ExecCommandParams`、`WriteStdinParams`；
  - 工具名常量：`EXEC_COMMAND_TOOL_NAME`、`WRITE_STDIN_TOOL_NAME`；
  - `create_*_tool_for_responses_api()`：供 Responses API 注册。

- 会话管理（`exec_command/session_manager.rs`）
  - `handle_exec_command_request(params)`：
    - 分配 `SessionId`，`create_exec_command_session(..)` 创建 PTY 会话：
      - 读任务：阻塞读取 PTY，广播到 `tokio::sync::broadcast`；
      - 写任务：接收 `writer_tx`，阻塞写入 PTY；
      - 等待任务：阻塞等待子进程退出并通过 oneshot 返回退出码；
    - 在 `yield_time_ms` 窗口内收集输出；若进程已退出，补一个短的“残留读取”以吸尽缓冲；
    - 以“字节上限≈`max_output_tokens*4`”做 UTF‑8 安全的“中部截断”（`truncate_middle`），并返回 `ExecCommandOutput`，状态为 `Exited(code)` 或 `Ongoing(session_id)`；
    - `result_into_payload(..)`：转换为 `FunctionCallOutputPayload`（文本 + 成功标记）。
  - `handle_write_stdin_request(params)`：
    - 按 `session_id` 找会话，写入 `chars`（若非空），然后在 `yield_time_ms` 内收集“从此刻起”的输出；
    - 同样采用中部截断并返回 `Ongoing(session_id)`。

- Shell 翻译（`codex.rs::maybe_translate_shell_command`）
  - 在 PowerShell 或启用 profile 的场景，把默认 shell 调用包装为用户 shell 期望的形式（`user_shell.format_default_shell_invocation(..)`）。

---

## 7) 补丁与审批：apply_patch.rs/tool_apply_patch.rs

- `apply_patch(..)` 决策：
  - `SafetyCheck::AutoApprove` → 返回 `InternalApplyPatchInvocation::DelegateToExec(..)`，由 exec 真正落地；
  - `AskUser` → 发送 `ApplyPatchApprovalRequestEvent`，按用户决策继续或拒绝；
  - `Reject { reason }` → 直接返回失败输出。
- `convert_apply_patch_to_protocol(..)`：将变更映射为 `protocol::FileChange`，供 UI 预览与审批。
- 在 `codex.rs::handle_container_exec_with_params(..)` 中识别 `apply_patch`：调用 `maybe_parse_apply_patch_verified(..)` 解析并走上述流程；若委托到 exec，会构造 `codex --codex-run-as-apply-patch <patch>` 的执行参数。

---

## 8) MCP 工具：mcp_connection_manager.rs/mcp_tool_call.rs

- `handle_mcp_tool_call(..)`：
  - 发送 `McpToolCallBegin`，调用 `sess.call_tool(server, tool, args, timeout)`，随后发送 `McpToolCallEnd`；
  - 将 `CallToolResult` 转为 `FunctionCallOutputPayload`（优先 structured_content，其次 content 文本块）。
- `openai_tools.rs`：`mcp_tool_to_openai_tool(..)` + `sanitize_json_schema(..)`：宽容处理 MCP 端给出的 schema（推断缺失 `type`，normalise integer→number，数组/对象补全默认等）。

---

## 9) 会话/历史与辅助

- 会话与历史：`conversation_manager.rs/conversation_history.rs/message_history.rs`
  - 记录回合输入输出、检索历史、拼接“当前历史+本回合输入”。
- 回合改动：`turn_diff_tracker.rs`
  - 在 `on_exec_command_begin/end` 之间跟踪工作区变更，便于 TUI 渲染“本回合修改”。
- 其余：`rollout.rs/git_info.rs/user_agent.rs/error.rs/user_notification.rs/terminal.rs/util.rs`
  - 版本/回滚记录、Git 信息、UA、错误分层与用户提示、终端能力与通用工具。

---

## 10) 典型调用链备忘

- 一次性执行
  - `codex.rs::handle_function_call("shell")` → `parse_container_exec_arguments` → 安全评估/审批 → `process_exec_tool_call` → `on_exec_command_end`（含格式化输出）→ 把 JSON 结果回填给模型
- 流式会话
  - `handle_function_call(EXEC_COMMAND_TOOL_NAME)` → `ExecSessionManager::handle_exec_command_request` → 返回 `Ongoing(session_id)` → 后续 `WRITE_STDIN_TOOL_NAME` 继续交互
- 补丁
  - `handle_container_exec_with_params(..)` 检测到 `apply_patch` → `apply_patch::apply_patch`（审批/委托）→ 若委托则以 `codex --codex-run-as-apply-patch` 形式执行 → `PatchApplyEnd`
- MCP
  - `handle_function_call("server/tool")` → `handle_mcp_tool_call(..)` → Begin/End 事件 + 结构化输出

---

## 11) 阅读建议与排错

- 先看函数签名/注释，再按一次完整回合“从提交到结果”的路径步步跟。
- 观察 `EventMsg` 的发送点，结合 TUI/CLI 的消费逻辑可快速还原端到端行为。
- 建议用测试（尤其 `openai_tools.rs`/`client_common.rs` 的单测、`exec_command` 的行为测试）验证理解；对于解析类逻辑（`parse_command.rs`），优先增设用例。

> 需要我继续针对某一条调用链（比如流式会话或补丁审批）画一张数据流图并补充到本文件吗？

