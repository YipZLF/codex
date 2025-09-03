# 多 Agent 统一端点方案：TUI 与 Headless 一致暴露 MCP 控制口

目标：无论子 Agent 是否带 TUI，都对外暴露同一类 MCP 控制端口（codex.* 工具），从而让 Bus/编排器以一致方式进行消息投递与启动/中断/结束控制，同时最大化复用 Codex 现有 core/TUI/MCP 能力。

---

## 痛点回顾（为何要统一）

- 现状差异：
  - Headless 模式（`codex-mcp-server`）：对外暴露 MCP 端口 → Bus/同伴可直接 `codex.reply/inject/interrupt/shutdown`。
  - TUI 模式（`codex-tui`）：没有对外暴露 MCP 端口 → 外部无法对“同一会话”进行控制/消息注入。
- 结果：两种形态的子 Agent 控制面不统一，不利于 Bus 的通用编排与跨机器伸缩。

---

## 统一方案总览

- 单一“Agent 端点”抽象：无论是否带 TUI，均暴露 MCP 端点（同一类工具：`codex.reply/inject/interrupt/shutdown/status`）。
- 两种运行形态，端点一致：
  1) Headless：仅运行 MCP 服务器（保持现状）。
  2) TUI+MCP：在同一进程中同时运行 TUI 与嵌入式 MCP 服务器（新能力）。
- 共享同一 `Session`：TUI 与 MCP 请求都发送到同一个 codex-core `Session`，从而保证“看到的就是被控制的”。

---

## 设计要点

### 1) 嵌入式 MCP 服务器（TUI 内）

- TUI 启动后，在同进程内“挂一个 MCP 监听端口”：
  - 传输推荐：`unix:///path/to/codex.sock`（同机安全、不会干扰终端）或 `tcp://127.0.0.1:PORT`（便于跨容器）。
  - 认证与权限：本地 socket 用文件权限控制；TCP 建议本机回环 + 可选 token。
- MCP 处理逻辑沿用 `codex-rs/mcp-server` 的 `MessageProcessor`/`codex_tool_runner` 等，实现最小重用。
- 关键差异：`MessageProcessor` 的“会话提供者”改为“注入现有 TUI 的 Session”，而不是自己新建会话。

### 2) 共享 Session 的调用路径

- TUI 初始化时通过 `Codex::spawn` 得到 `(sess, turn_context)`；
- 嵌入式 MCP server 接收 JSON‑RPC 请求，执行：
  - `codex.reply` → 转换为 `Op::UserInput` 提交到 `submission_loop`
  - `codex.inject` → 调用 `sess.inject_input()`（或封装为统一 `Op`）
  - `codex.interrupt` → `Op::Interrupt`
  - `codex.shutdown` → `Op::Shutdown`
  - `codex.status` → 读取当前任务、会话信息
- 审批/工具/差异/日志：仍由同一个 Session 产出事件，TUI 原生渲染不变。

### 3) CLI 与配置

- 为 `codex-tui` 增加可选参数：
  - `--mcp-bind <addr>`：如 `unix:///tmp/codex-agent.sock` 或 `tcp://127.0.0.1:6006`
  - `--mcp-auth <token>`（可选）：简单鉴权
- 默认关闭，调试/编排需要时开启；Headless 则继续使用 `codex-mcp-server` 按现状对外。

### 4) 并发与稳定性

- TUI UI 线程与 MCP 处理在不同 Tokio 任务中运行；
- 所有 MCP 调用最终转成 `Op::*` 或 `sess.inject_input()`，与来自人类输入/模型调用的路径一致；
- `interrupt/shutdown` 与 TUI 的热键/按钮等价；两者不会互相踩踏（core 已处理 `TurnAbortReason`）。

---

## 关键改造点（最小化）

- mcp-server：
  - 提取“传输层”→ 允许除 stdio 外使用 Unix/TCP；
  - 提取“会话提供者”→ 新增一种“使用外部注入的 Session”。
- codex-tui：
  - 解析 `--mcp-bind` 后，启动一个“嵌入式 MCP 监听任务”；
  - 用 `Arc<Session>` 注入给 `MessageProcessor` 作为会话目标。
- core：
  - 无需业务逻辑修改；确保 `Session` 的 `submit` 与 `inject_input/interrupt` 在多源并发下稳定（现有实现已支持）。

---

## 与 Bus 的配合

- Bus 保持“只承载内容消息 + 最小控制”的职责：
  - 内容：`bus.publish` → 对订阅者执行 `codex.reply/inject`
  - 控制：`bus.control` → 对订阅者执行 `codex.interrupt/shutdown/start`
- 对 Bus 与观察者而言，“子 Agent 是否带 TUI”已透明：端点一致，统一用 MCP 工具访问即可。

---

## 运行形态与建议

- 生产/自动化：子 Agent 通常无头运行；观察者按需启 TUI 或不开。
- 调试/现场观察：对关键 Agent 开 TUI 并 `--mcp-bind`，Bus/同伴仍可通过 MCP 控制它；人类可在 TUI 里看到同一会话的全部细节。
- tmux：仅在需要并行盯多个带 TUI 的会话时使用；对系统架构不是必需品。

---

## 安全与跨机器

- 同机优先用 Unix Socket 并设置 0600 权限；
- 跨容器/跨主机：
  - 起步：SSH 将远端 `codex-tui --mcp-bind unix:///...` 的 socket 通过隧道转发；
  - 工程化：网关/Sidecar 提供 `wss://`/`tcp://` 入口，将网络帧转为本地 socket 的 JSON‑RPC 行。

---

## 路线图（实施顺序）

1) mcp-server 抽象传输与会话提供者；保留 stdio 模式不变。
2) codex-tui 增加 `--mcp-bind`，在同进程内启动 MCP 服务并绑定到现有 Session。
3) 回归测试：
   - 无头/带 TUI 两模式下，Bus 对子 Agent 的 `reply/inject/interrupt/shutdown` 行为应一致；
   - 审批/工具/差异/日志在 TUI 中可见；
   - 竞争测试：人类/TUI 与外部 MCP 同时注入输入与中断，系统保持一致性。

---

## 小结

通过“在 TUI 进程中嵌入 MCP 端点”，统一了子 Agent 的控制面：无论 Headless 还是带 TUI，对外都是同一个 MCP 工具集（codex.*），Bus/编排器一致访问。同一 Session 同时服务 UI 与外部控制，请求路径与事件渲染保持原味，最大化复用 Codex 现有能力且易于扩展到跨机器部署。
