# Codex Core 源码导读（建议阅读顺序）

本文面向首次接触 Codex 的读者，聚焦 `codex-rs/core`（crate: `codex-core`）。目标是在较短时间内建立对核心运行机制的心智模型，再按需下钻到实现细节；读完本导读，你应能顺着一次“提交→模型→工具→执行/流式→事件回传→补丁/历史记录”的完整主线把握代码流向。

---

## 总览与术语

- 核心定位：`codex-core` 负责会话编排、模型请求与工具调用、执行/沙箱集成、补丁应用、MCP 调用、事件流转与历史追踪。
- 对外协议：类型定义来自 `codex-protocol`（被 core 重导出为 `codex_core::protocol`）。外层的 TUI/CLI 通过协议事件与 core 交互。
- 关键对象：
  - 提交 Submission → 事件 Event（流式）
  - 模型 Prompt/ResponseEvent（到 OpenAI/MCP）
  - 工具 Tool（shell/streamable shell/apply_patch/MCP/plan 等）
  - 执行链 Exec（一次性）与 ExecCommand（PTY 会话，支持 stdin）

---

## 推荐阅读顺序（从全局到主线）

1) 入口与总线：`core/src/lib.rs` → `core/src/codex.rs`
   - 了解模块地图、事件/提交通道、`Codex::spawn` 初始化与事件派发。
2) 配置与协议上下文：`config.*` → 略读 `codex-protocol`
   - 先掌握配置（模型/审批/沙箱/工作目录/指令注入等），再回看 codex 主循环更清晰。
3) 模型与工具挂载：`client_common.rs` → `client.rs` → `model_*` → `openai_tools.rs`
   - 理解 Prompt 构造、工具清单、不同模型族与工具差异（Responses API、本地壳、web_search、view_image、apply_patch）。
4) 指令与计划：`parse_command.rs`、`plan_tool.rs`
   - 模型/用户给出的命令如何被解析与总结，如何进入“计划工具”。
5) 执行与沙箱（一次性执行链）：`exec.rs` → `exec_env.rs/shell.rs/spawn.rs` → `safety.rs/is_safe_command.rs` → `seatbelt.rs/landlock.rs`
   - 命令如何落地到子进程、如何限流/截断输出、如何在 macOS/Linux 沙箱内运行及错误映射。
6) 流式会话（PTY + stdin）：`exec_command/mod.rs`（含 `session_manager.rs` 等）
   - 长运行命令的流式输出/会话化 stdin 写入、窗口化“读取最新输出”、中部截断标记。
7) 补丁与 MCP：`apply_patch.rs/tool_apply_patch.rs`、`mcp_connection_manager.rs/mcp_tool_call.rs`
   - 审批/自动批准策略、如何桥接 `codex-apply-patch`；MCP 工具的调用与事件上报。
8) 会话/历史与辅助：`conversation_manager.rs/conversation_history.rs/message_history.rs` → `turn_diff_tracker.rs` → 其余（`rollout.rs/git_info.rs/user_agent.rs/error.rs/user_notification.rs/terminal.rs/util.rs`）

> 建议“宽读函数签名+注释”，随后按一次完整任务流追踪到具体实现；碰到复杂点（如 PTY 或沙箱）先看公共接口，再看平台细节。

---

## 模块解读（要点速览）

### 入口/编排
- `lib.rs`
  - 导出核心模块与常用别名，重导出 `codex-protocol`（保持 `codex_core::protocol::*` 的兼容）。
- `codex.rs`
  - 结构：`Codex`（提交/事件通道）、`Codex::spawn`（构建 `Session`，注入 `ConfigureSession`）、事件主循环。
  - 关注：`INITIAL_SUBMIT_ID`、事件路由、工具调用入口、流式/非流式执行分发、错误到 UI 的映射。

### 配置与协议上下文
- `config.rs/config_types.rs/config_profile.rs/flags.rs/environment_context.rs`
  - 模型提供方、推理级别、审批/沙箱策略、工作目录、基础指令与用户指令、格式化/摘要选项。
- 协议（`codex-protocol`）
  - 只需略读核心类型：`Event/EventMsg/Submission/ResponseItem`、工具相关事件、`SandboxPolicy/AskForApproval` 等。

### 模型与工具挂载
- `client_common.rs`
  - `Prompt`（输入、工具清单、是否持久化、BASE_INSTRUCTIONS 拼装）；`ResponseEvent` 流；verbosity/reasoning 控制。
  - 关键：当没有覆盖说明时，是否自动追加 `apply_patch` 的使用说明；文本/工具参数如何落到 Responses API。
- `client.rs`
  - 组织请求与事件解包（配合 `ResponseStream`）。
- `model_family.rs/model_provider_info.rs/openai_model_info.rs`
  - 模型族能力差异（是否使用本地 shell、是否需要特殊 apply_patch 指令、是否支持 reasoning summary 等）。
- `openai_tools.rs`
  - 构建工具清单（`ToolsConfig`/`ToolsConfigParams`）：本地壳、带审批壳、流式壳、apply_patch（freeform/function）、web_search、`view_image`；
  - MCP 工具 schema 清洗与稳定排序（保证 schema 中 `type` 存在、数组/对象默认项补全等）。

### 指令解析与计划
- `parse_command.rs`
  - 将任意命令解析为“可读摘要”类别（Read/ListFiles/Search/Format/Test/Lint/Unknown/Noop），事件层面统一到协议的解析类型。
- `plan_tool.rs`
  - update_plan 的处理（TUI 会消费）。

### 一次性执行链（非流式）
- `exec.rs`
  - 入口：`process_exec_tool_call(params, sandbox_type, sandbox_policy, codex_linux_sandbox_exe, stdout_stream)`
  - 行为：根据平台/策略选择 Seatbelt/Landlock/无沙箱；聚合 stdout/stderr（限制 live 事件数量）、捕获退出码、对“疑似沙箱拒绝”做保守判断；统一超时/杀死逻辑。
  - 输出：`ExecToolCallOutput { exit_code, stdout, stderr, aggregated_output, duration }`
- `exec_env.rs/shell.rs/spawn.rs`
  - 环境构造、命令拼装、`spawn_child_async`；I/O 重定向策略（工具流式输出事件的来源）。
- `safety.rs/is_safe_command.rs`
  - 风险评估、白/黑名单；在 `apply_patch` 与 `shell` 提权说明里也会用到。
- `seatbelt.rs/landlock.rs`
  - 平台沙箱封装与错误映射（macOS Seatbelt、Linux Landlock+seccomp）。

### 流式会话执行（PTY + stdin）
- `exec_command/mod.rs`
  - 重要导出：`ExecSessionManager`、`ExecCommandParams`、`WriteStdinParams`、工具名常量 `EXEC_COMMAND_TOOL_NAME`/`WRITE_STDIN_TOOL_NAME`。
- `session_manager.rs`
  - `handle_exec_command_request`：创建 PTY 会话、读取到“屈服时间（yield_time_ms）”或进程退出；按字节上限估算 token 并“中部截断”，返回 `ExecCommandOutput`（`Exited` 或 `Ongoing(session_id)`）。
  - `handle_write_stdin_request`：向指定会话写入 chars，窗口化读取“从现在开始”的输出，仍按上限中部截断，保持 `Ongoing(session_id)`。
  - `truncate_middle`：UTF-8 安全的中部截断，过小上限时返回完整的“…N tokens truncated…”标记。
- `exec_command_session.rs`
  - 会话对象封装：writer 通道（stdin）、broadcast 通道（输出）、killer 与读写任务句柄、子进程等待。
- 读/写/等待三任务：阻塞线程读取 PTY、tokio 写入 stdin、阻塞等待 exit，将退出码通过 `oneshot` 传回。

### 补丁与审批
- `apply_patch.rs/tool_apply_patch.rs`
  - 依据 `SafetyCheck`（`AutoApprove/AskUser/Reject`）决定：直接返回文本输出、或委托到 `exec` 执行（带 `--codex-run-as-apply-patch`），或拒绝并返回原因。
  - `convert_apply_patch_to_protocol`：将差异映射为协议层 `FileChange`，便于 TUI 渲染与审批。

### MCP 工具
- `mcp_connection_manager.rs/mcp_tool_call.rs`
  - 组织一次 MCP 工具调用，发送 `McpToolCallBegin/End` 事件，调用 `sess.call_tool(server, tool, args, timeout)`，将结果封装回 `ResponseInputItem`。

### 会话/历史与辅助
- `conversation_manager.rs/conversation_history.rs/message_history.rs`
  - 会话生命周期管理、消息历史聚合。
- `turn_diff_tracker.rs`
  - 跟踪每回合文件改动，配合 TUI 做可视化。
- 其他：`rollout.rs/git_info.rs/user_agent.rs/error.rs/user_notification.rs/terminal.rs/util.rs`
  - 版本/回滚记录、Git 信息、UA、错误分层与用户提示、终端能力与通用工具。

---

## 关键类型/函数速查

- `Codex::spawn(config, auth_manager, initial_history)`：创建会话、建立通道、发送 `ConfigureSession`。
- `process_exec_tool_call(..)`：一次性执行入口（含沙箱/超时/输出聚合）。
- `ExecSessionManager::handle_exec_command_request(..)`：开启 PTY 会话、收集初始窗口输出。
- `ExecSessionManager::handle_write_stdin_request(..)`：向会话写入并收集新窗口输出。
- `ToolsConfig::new(..)`/`get_openai_tools(..)`：决定向模型暴露哪些工具及其 JSON‑Schema。
- `apply_patch(..)`：补丁审批与执行路径选择。
- `handle_mcp_tool_call(..)`：一次 MCP 工具调用的事件与结果封装。

---

## 如何顺藤摸瓜定位代码

- 从 `codex.rs` 搜索 `Tool`/`ExecCommand`/`ApplyPatch`/`Mcp` 关键字，进入对应分支。
- 路径样例（一次性执行）：`codex.rs → openai_tools.rs → exec.rs → spawn.rs/seatbelt.rs/landlock.rs → error.rs`
- 路径样例（流式会话）：`codex.rs → openai_tools.rs (StreamableShell) → exec_command/* → session_manager.rs`
- 路径样例（补丁）：`codex.rs → tool_apply_patch.rs/apply_patch.rs → codex-apply-patch`

---

## 注意事项与阅读小贴士

- 代码风格：`format!` 能内联变量的地方统一使用 `{}` 内联。
- 环境常量：切勿改动与沙箱环境变量相关的逻辑（例如集成测试条件早退）。
- 输出策略：一次性执行对 live 事件做上限限制，但聚合输出保留完整；流式会话使用“中部截断”并在输出中标注原 token 估值。
- 复杂路径（PTY/沙箱）：先看类型与接口，再看平台实现细节，最后读测试用例验证行为。

---

## 读完 core 之后的下一步

- `codex-rs/tui`：
  - 关注快照测试（`insta`）、渲染分层（组件/渲染器/样式）、对协议事件的消费与状态管理。
- `codex-rs/cli`：
  - clap 子命令、与 `codex-arg0` 的集成、`codex-tui`/`codex-exec` 的分流与参数覆盖传递。

> 如需，我可以基于本文顺序继续“带读”具体文件与关键函数实现。

