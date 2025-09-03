# 多 Agent 消息总线改造方案与里程碑（MCP + TUI 复用）

本文归纳多 Agent 架构的目标、里程碑与交付节奏，面向工程落地与团队协作。

---

## 总体目标

- 统一子 Agent 的对外控制端口（无论 Headless 还是带 TUI），以同一套 MCP 工具访问。
- 引入独立消息总线（Bus），仅承载内容消息的发布/订阅与最小控制（start/interrupt/shutdown）。
- 最大化复用 Codex 现有 core/TUI/MCP 能力，保留原生 TUI 的调试体验，并支持跨机器伸缩。

---

## 里程碑 M0：契约与原型冻结（1–2 天）

- 契约确认（工具与参数）
  - codex.*：`codex.reply`（UserInput）、`codex.inject`（注入输入）、`codex.interrupt`、`codex.shutdown`、`codex.status`。
  - bus.*：`bus.subscribe(topic, agent{server,session_id?}, delivery)`、`bus.publish(topic, items, headers)`、`bus.unsubscribe`、`bus.control(action, agent/spec)`。
- 文档与示例：完成工具 JSON 约定与最小示例（含文本 InputItem 映射与 correlation_id）。
- 输出：契约文档 + 示例调用 JSON；选定初期传输（本地/SSH）。

---

## 里程碑 M1：Codex MCP 工具标准化（2–3 天）

- mcp-server：补齐 codex.* 工具与 Op 映射（reply→UserInput、inject→sess.inject_input、interrupt/shutdown→Op）。
- 验证：单会话流程（工具调用→事件流），不改 TUI 渲染路径。
- 验收：本地起服，外部 mcp-client 调用 codex.* 工具生效。

---

## 里程碑 M2：TUI 嵌入 MCP 端点（统一端口）（3–5 天）

- codex-tui：新增 `--mcp-bind <addr>`（`unix:///tmp/codex.sock` 或 `tcp://127.0.0.1:PORT`），同进程起“嵌入式 MCP 服务器”，绑定现有 Session。
- mcp-server：抽象“传输层”与“会话提供者”，支持注入外部 Session。
- 验收：带 TUI 场景仍可由外部 mcp-client 调用 codex.* 工具；UI 与外部控制操纵同一会话，互不冲突。

---

## 里程碑 M3：Bus MVP（独立模块）（5–7 天）

- 新 crate：`codex-mcp-bus`（或独立仓库）
  - 内存订阅表（topic → 订阅者集合）与简单幂等（correlation_id 去重缓存）。
  - 工具实现：`bus.subscribe/publish/unsubscribe/control`。
  - 投递：对订阅者执行反向 MCP 调用（delivery=inject/user_input；control→interrupt/shutdown/start）。
  - 可靠性：超时/重试（固定策略），失败计数；DLQ 为下一里程碑。
- CLI：`codex-bus` 支持加载配置（订阅限流/最大重试）、日志与简要指标。
- 验收：三角色联通（观察者 TUI / 子 Agent / Bus），发布→子 Agent 收到并进入对话；control 生效。

---

## 里程碑 M4（可选）：观察者 TUI 便捷命令（1–2 天）

- TUI：
  - `/subscribe <topic> [inject|user_input]` → 调 Bus.subscribe。
  - `/publish <topic> <text|payload>` → 调 Bus.publish。
- 验收：人类不依赖“模型先学会使用工具”即可短链接入与发消息。

---

## 里程碑 M5：跨机器传输与网关（3–5 天）

- 起步方案（零改代码）：SSH 透传 stdio（在 `mcp_servers.*` 的 command/args 写成 `ssh user@host <远端可执行>`）。
- 工程化选项（原型）：轻量网关 `mcp-net-gateway`（stdio ↔ TCP/WS），统一 TLS/mTLS/令牌。
- 验收：跨主机子 Agent（Headless/TUI+MCP）可被 Bus 控制与收发。

---

## 里程碑 M6：可靠性与治理（5–7 天）

- Bus：
  - DLQ、重试策略（指数回退/限次）、分区（hash(topic/key)）。
  - 指标：投递延迟、重试、队列深度、吞吐；trace/correlation_id 贯穿。
  - ACL：topic-level 与 control-level 权限（白/黑名单）。
- 验收：压测下稳定；错误与背压有可观测。

---

## 里程碑 M7：扩展与落地（持续）

- 状态存储（可选）：订阅表/消息索引持久化（SQLite/Redis/PG），支持回放。
- 横向扩展：Bus 前端无状态 + 后端存储/队列；订阅者分片。

---

## 代码触点（预估）

- mcp-server：`codex-rs/mcp-server/src/{message_processor.rs,codex_tool_runner.rs,...}`（工具映射、会话提供者抽象、传输适配）。
- tui：`codex-rs/tui/src/{main.rs,lib.rs,slash_command.rs}`（解析 `--mcp-bind`、嵌入式 MCP 监听任务、可选 slash 命令）。
- bus（新）：`codex-rs/bus`（或独立仓库）：核心路由、工具 schema、CLI。
- 公共：文档（运行手册、配置样例、SSH 方案、网关说明）。

---

## 测试与验收

- 单元：
  - codex.* 工具与 Op 映射、并发注入/中断一致性。
  - Bus 订阅/发布/控制与失败重试。
- 集成：
  - Headless 与 TUI+MCP 均能被 Bus 统一控制与投递。
  - 跨机器（SSH）演示；可选网关自测。
- TUI 快照：
  - 若新增 slash 命令的提示/回显，更新 `codex-tui` 的 snapshot。

---

## 风险与缓解

- 并发竞争（TUI 与外部 MCP 同时注入/中断）：依赖 `Session` 既有一致性；补充回归测试。
- 传输稳定性（SSH/网关）：起步走 SSH；网关作为增量选项，先小流量试点。
- 安全：默认 Unix Socket + 强权限；跨机用 SSH/mTLS；Bus/控制工具加 ACL。

---

## 交付节奏

- 先内聚 Codex 侧（M1/M2），再做 Bus MVP（M3），最后扩展跨机与可靠性（M5/M6）。
- 每个里程碑具备可演示的最小路径（demo 命令/脚本）。

