# Codex 的 MCP Server/Client 实现综述（自顶向下）

本文系统性梳理 Codex 在 MCP（Model Context Protocol）方向的“服务端 + 客户端”能力、交互流程，以及关键代码位置，便于开发/编排/扩展。

---

## 1. 总览

- 场景定位
  - MCP Server：为上层编排器/IDE/代理提供标准化 JSON‑RPC（stdio）接口，在单进程内创建与管理多条 Codex 会话（多 Agent）。
  - MCP Client：轻量异步客户端，用于从父进程拉起任意 MCP 服务器（含 Codex 自己）并与之通信（initialize、tools/list、tools/call、订阅等）。

- 通信形态
  - 服务器/客户端均以“行分隔 JSON‑RPC 消息”在 stdio 上传输；客户端负责请求 ID 配对与超时处理；服务器负责请求分发与事件通知。

---

## 2. MCP Server：结构与数据流

- 入口与主循环
  - 文件：`codex-rs/mcp-server/src/main.rs`（简单入口） → `codex-rs/mcp-server/src/lib.rs::run_main`
  - run_main 工作：
    - 安装日志；解析 `-c key=value` 覆盖并加载 `Config`；
    - 建立三类异步任务：
      1) stdin_reader：逐行读取 JSON，反序列化为 `JSONRPCMessage`，发往 `incoming_rx`；
      2) processor：构造 `MessageProcessor`，从 `incoming_rx` 接收并按类型分发（请求/响应/通知）；
      3) stdout_writer：从 `outgoing_rx` 接收 `OutgoingMessage`，序列化为 `JSONRPCMessage` 写 stdout。

- 消息处理（MCP 标准方法 + Codex 扩展）
  - 文件：`codex-rs/mcp-server/src/message_processor.rs`
  - 结构：
    - `MessageProcessor` 持有 `OutgoingMessageSender`、`ConversationManager`、`CodexMessageProcessor` 等；
    - `process_request`：优先尝试将请求 JSON 映射为 Codex 自有 `codex_protocol::mcp_protocol::ClientRequest`（即“Codex 扩展方法”）；否则转换为 `mcp_types::ClientRequest`（MCP 标准方法）并分派到对应 `handle_*`；
    - `process_response`/`process_notification`：分别处理 JSON‑RPC Response/Notification，按需回传给客户端或记录日志。
  - MCP 标准方法中已实现的关键处理：
    - `tools/list` → 返回 Codex 专属工具清单（“codex”“codex-reply”）。
    - `tools/call` → 调度到 Codex 工具调用路径（见下）。

- Codex 工具与会话编排
  - 工具 schema 与入参 → `codex-rs/mcp-server/src/codex_tool_config.rs`
    - `CodexToolCallParam`（工具“codex”）：包含 `prompt`、`model`、`profile`、`cwd`、`approval-policy`、`sandbox`、`config: {k: v}`、`base_instructions`、`include_plan_tool` 等；
      - `into_config(codex_linux_sandbox_exe)`：将上述参数折叠为有效的 `codex_core::config::Config`；`config` 字段会映射为 dotted‑path 覆盖（等价 `-c`）。
    - `CodexToolCallReplyParam`（工具“codex-reply”）：对已有 `session_id` 续写 `prompt`。
    - `create_tool_for_*`：生成工具的 JSON Schema，供 `tools/list` 返回。
  - 工具调用执行 → `codex-rs/mcp-server/src/codex_tool_runner.rs`
    - `run_codex_tool_session(id, initial_prompt, config, ...)`：
      - `ConversationManager::new_conversation(config)` → 新建会话，发送 `SessionConfigured` 事件（以 MCP Notification 形式转发给客户端）。
      - 将原始 MCP `request_id` 作为 Codex `Submission.id`，发送 `Op::UserInput{ Text }`。
      - 循环 `conversation.next_event()`：把 core 事件（EventMsg）转封为 MCP Notification；
        - 遇到 `ExecApprovalRequest`/`ApplyPatchApprovalRequest`：调用 `exec_approval.rs`/`patch_approval.rs` 发起 elicit 请求并等待客户端答复，再继续执行；
        - 遇到 `TaskComplete`：构造 `CallToolResult`（文本结果）并以 `tools/call` 响应结束本次调用；
        - 遇到错误：返回 `CallToolResult { is_error: true }`。
    - `run_codex_tool_session_reply(...)`：在已有 `CodexConversation` 上复用会话，支持多轮。
  - Codex 扩展方法 → `codex-rs/mcp-server/src/codex_message_processor.rs`
    - 提供 `NewConversation`/`SendUserMessage`/`SendUserTurn`/`InterruptConversation` 等自有 RPC（在 `message_processor.rs` 中优先识别处理）。

- 审批桥接（elicit）
  - `codex-rs/mcp-server/src/exec_approval.rs`、`patch_approval.rs`
    - 将 core 的 `ExecApprovalRequestEvent`/`ApplyPatchApprovalRequestEvent` 转为 MCP 侧的 elicit 请求，等待客户端回传 `Approved/Denied/...` 决策，再通知 core 继续/中止。

- 对外通知编码
  - `codex-rs/mcp-server/src/outgoing_message.rs`：统一封装 JSON‑RPC 消息的发送；支持在 `notification.params._meta` 中加入 MCP 规范允许的附加信息（请求配对/跟踪）。

---

## 3. MCP Client：结构与用法

- 位置：`codex-rs/mcp-client/src/mcp_client.rs`
- 能力：
  - `McpClient::new_stdio_client(program, args, env)`：拉起子进程（MCP 服务器），清空 env 并注入必要的 MCP 变量，建立 stdio 通道；
  - `initialize(params, notification_params, timeout)`：发送 `initialize` 请求，再发送 `notifications/initialized`；
  - `list_tools(params, timeout)`：`tools/list` 便捷包装；
  - `call_tool(name, args, timeout)`：`tools/call` 便捷包装；
  - `send_request<R: ModelContextProtocolRequest>`/`send_notification<N: ModelContextProtocolNotification>`：泛型请求/通知接口；
  - 内部：
    - 后台 writer 任务：从 `outgoing_tx` 读取 `JSONRPCMessage` 写入子进程 stdin；
    - 后台 reader 任务：逐行读子进程 stdout，反序列化 `JSONRPCMessage`，依据 `id` 匹配 pending 请求或投递到 `responses` 通道；
    - `pending` 哈希表管理请求 ID → oneshot 发送端，确保响应配对与超时回收。

- 典型用法（伪代码）
  1) `let client = McpClient::new_stdio_client("codex", vec!["mcp"], None).await?;`
  2) `client.initialize(init_params, None, Some(Duration::from_secs(30))).await?;`
  3) `let tools = client.list_tools(None, Some(timeout)).await?;`（找到 `codex`/`codex-reply`）
  4) `client.call_tool("codex", arguments, Some(timeout)).await?;`（新建会话并执行）
  5) 若服务器提供“订阅/监听”机制，可通过 Codex 扩展方法 `AddConversationListener` 等订阅事件通知。

---

## 4. 端到端交互示意

1) Orchestrator（MCP 客户端）拉起 Codex MCP 服务器 → `initialize`
2) `tools/list` → 发现 Codex 工具；
3) `tools/call(name="codex", arguments=CodexToolCallParam)` → 服务器创建会话并发送 `SessionConfigured`/事件流通知；
4) 若有审批事件 → 服务器发起 elicit，客户端决策后回传；
5) 完成一轮后 → 服务器在 `TaskComplete` 时返回 `CallToolResult` 作为 `tools/call` 的响应；
6) 若需续写 → `tools/call(name="codex-reply", arguments=CodexToolCallReplyParam)`，或使用 Codex 扩展 RPC。

---

## 5. 与 TUI 的关系

- TUI 是“core 事件的图形化消费者与人机入口”，MCP Server 是“程序化接口”；两者都以 codex-core 为内核，彼此解耦：
  - TUI 直接驱动 core，现场渲染；
  - MCP Server 将 core 的事件封装为 Notification，供客户端处理/再展示（可自研“监控 TUI”作为客户端）。

---

## 6. 可扩展点与建议

- 扩展工具：在 `codex_tool_config.rs` 增添新工具及 schema，并在 `message_processor.rs::handle_call_tool` 中分派。
- 事件细化：在 `codex_tool_runner.rs` 中对 EventMsg 进行更精细的转译（例如将增量文本/推理在 MCP 中分通道呈现）。
- 安全/审批：根据场景把审批策略设为 `AskForApproval::Never` 或保留 elicit 人机在环；
- 监控/观测：在 orchestrator 侧统一收集 Notification，记录到日志/指标系统，便于审计与复现。

---

## 7. 代码地图（速查）

- 服务器入口与主循环：`mcp-server/src/lib.rs::run_main`
- MCP 标准方法分发：`mcp-server/src/message_processor.rs`
- Codex 扩展 RPC：`mcp-server/src/codex_message_processor.rs`
- Codex 工具参数与 schema：`mcp-server/src/codex_tool_config.rs`
- 工具调用执行/事件转发：`mcp-server/src/codex_tool_runner.rs`
- 审批桥接（elicit）：`mcp-server/src/exec_approval.rs`、`mcp-server/src/patch_approval.rs`
- 出站消息封装：`mcp-server/src/outgoing_message.rs`
- 轻量客户端：`mcp-client/src/mcp_client.rs`

