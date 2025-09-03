# 使用 Codex 构建多 Agent 与 MCP/TUI 协同系统的实践说明

本文面向开发人员，回答以下问题并给出可落地的设计：
- Codex 作为 MCP 服务端启动，与通过 TUI 启动有何区别？是否能“级联”启动子 Agent？
- 启动 MCP 服务时，是否能自定义 MCP 的配置、命令行参数（含 `-c` 覆盖）？
- 若既想让程序编排（MCP）自动运行，又想让人类查看/干预多个子 Agent 的细节，能否同时开启 TUI 与 MCP，并用 tmux 管理多个 TUI 会话？系统可行性与设计方案？

---

## 1. MCP 服务端 vs TUI：职责、交互与差异

- 职责定位
  - MCP 服务端（codex-rs/mcp-server）
    - 面向“程序/编排器”的 RPC 接口，实现 Model Context Protocol（JSON‑RPC over stdio）。
    - 可在一个进程内创建/管理多条 Codex 会话（ConversationManager）。
    - 审批（exec/patch）通过 MCP 的 elicit/请求-响应路径交给客户端决策。
  - TUI（codex-rs/tui）
    - 面向“人类用户”的控制台 UI。消费 core 的事件流（EventMsg），做渲染、输入与审批交互。

- 交互模式
  - MCP：上层 orchestrator（或 IDE/工具）作为客户端连接本进程的 MCP 服务器，发起指令（新建会话、发送用户输入、订阅事件等）。
  - TUI：本地直接驱动 codex-core，渲染输出，处理人类审批。

- 多会话能力与“级联”
  - 单服务器多会话：MCP 服务器内部用 `ConversationManager` 管理多会话，天然支持“多 Agent”。
  - 级联（服务器套服务器）：技术上可行（仓库有 `mcp-client` 可从父进程拉起子 MCP 服务器），但通常没必要。首选方案是“单 MCP 服务器 + 多会话”，足以表达“主 Agent 编排多个子 Agent”。

---

## 2. MCP 启动与自定义配置（含 `-c` 覆盖）

- 进程级配置
  - codex CLI 在所有子命令上支持 `-c key=value` 覆盖（通用配置覆盖，见 codex-rs/common）。
  - 启动 MCP 服务端时可传入：
    - `model`、`approval_policy`、`sandbox_mode`、`experimental_resume` 等任意配置字段。
  - 示例：
    - `codex mcp -c model="gpt-4.1-mini" -c approval_policy=never -c experimental_resume="/abs/path/rollout-....jsonl"`

- 会话级配置（每次工具调用）
  - MCP 工具 “codex” 的入参 `CodexToolCallParam` 支持：
    - 显式字段：`model`、`profile`、`cwd`、`approval-policy`、`sandbox`、`base_instructions`、`include_plan_tool`。
    - 通用覆盖：`config: { "foo.bar": <json> }`，会在服务器端转为 `-c` 覆盖等效的 dotted‑path。
  - 服务端将这些合成为“会话有效配置”，从而实现“单服务器不同会话不同配置”。

---

## 3. 同时开启 TUI 与 MCP，用 tmux 管理多个 TUI 会话

- 并行运行的可行性
  - MCP 服务器通过 stdio 通道与客户端连接（不是固定端口），可与多个 TUI 进程并行。
  - 现有 TUI 默认直接创建本地会话，不会“附着”到 MCP 服务器进程内已有会话；要做“附着/监控”，应实现一个作为 MCP 客户端的 TUI/监控器（见下）。

- 两条实践路径
  - 路径 A（推荐，人机分离）
    - 用 MCP 服务器承载“编排/执行”，每个子 Agent = 一条会话。
    - 开发一个“作为 MCP 客户端的轻量 TUI/监控器”来观察/控制：
      - 连接 MCP → `initialize`
      - `tools/list` → 发现 “codex”、“codex-reply” 等工具
      - 新建会话/发送输入：`tools/call`（codex / codex-reply）
      - 订阅事件：`AddConversationListener`，渲染 Agent 增量输出、审批请求等
    - 优点：
      - 单进程集中管理；事件天然结构化；审批流一致（通过 client 处理）。
  - 路径 B（tmux 多 TUI 直接看每个 Agent）
    - 每个子 Agent 启动一个独立 TUI 进程，tmux 管理多个 pane。
    - 编排器若要控制这些 Agent，需要额外的进程间通信或脚本，自动化复杂度更高。

- 综合建议
  - 生产/自动化：采用路径 A。
  - 研发/调试：路径 B 便于即时观察与人工干预。

---

## 4. 设计落地建议

- 多 Agent（MCP 服务器 + 多会话）
  - 主 orchestrator（MCP 客户端）负责：
    - 为每个任务/子 Agent 构造 `CodexToolCallParam`，使用不同的 `model/approval/sandbox/cwd`。
    - 订阅事件并执行业务决策（例如一个子 Agent 产出工具请求，主控决定是否批准或再分派到另一个子 Agent）。
  - 如需观测：编写 MCP 客户端 TUI（或在 tmux 另起面板运行人类向导的 TUI 版本）。

- 级联（仅在强隔离/资源控制需要时使用）
  - 父 orchestrator 使用 `mcp-client` 拉起子 MCP 服务器（不同工作目录/权限），父端将任务分配给不同子服务器；代价是进程管理与监控复杂度更高。

- 审批与安全
  - 自动化场景下建议设置 `AskForApproval::Never`，并使用 `SandboxMode::ReadOnly` 或 `WorkspaceWrite`；
  - 对高风险工具调用，引入“审批代理”子 Agent 作为人类/策略的代理人，通过 MCP elicit 接口返回决策。

---

## 5. 代码位置（便于进一步扩展）

- MCP 服务器入口与主循环
  - `codex-rs/mcp-server/src/lib.rs::run_main`：解析 `-c` 覆盖、加载 Config、启动 JSON‑RPC 读写任务与消息处理器。
  - `codex-rs/mcp-server/src/message_processor.rs`：协议层请求/通知分发；事件通知编码与发送。
  - `codex-rs/mcp-server/src/codex_message_processor.rs`：将 MCP 请求映射到 codex-core 的会话创建、输入发送、事件转发、审批桥接等。
- codex 工具定义与会话配置
  - `codex-rs/mcp-server/src/codex_tool_config.rs`：`CodexToolCallParam`/`CodexToolCallReplyParam` schema 及 `into_config()` 合成会话配置。
  - `codex-rs/mcp-server/src/codex_tool_runner.rs`：执行 codex 工具调用的具体逻辑（新建/续写会话、事件订阅等）。
- 审批桥接（elicit）
  - `codex-rs/mcp-server/src/exec_approval.rs`、`patch_approval.rs`：将 core 的审批事件转为 MCP 的 elicit 请求/响应。
- MCP 客户端（若做编排器/监控）
  - `codex-rs/mcp-client/src/mcp_client.rs`：
    - `McpClient::new_stdio_client(program, args, env)`：拉起子 MCP 进程并建立 stdio 通道。
    - `initialize()`、`list_tools()`、`call_tool()`、泛型 `send_request`/`send_notification`；内部管理请求配对、超时与 JSON‑RPC 编解码。

---

## 6. 小结

- MCP 服务端用于“对外协议与多会话编排”，TUI 面向人机交互；两者解耦良好。
- 多 Agent 系统首选“单 MCP 服务器 + 多会话”；如需进程隔离，可由 orchestrator 使用 mcp-client 级联拉起子服务器。
- 配置支持进程/会话两级定制：进程级用 `-c` 覆盖、会话级用 codex 工具参数/内嵌 `config` 字段。
- 人机并行观察可通过“自研 MCP 客户端 TUI”或“tmux 多 TUI 进程”两路满足。

