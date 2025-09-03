# 多 Agent 消息总线 – M0 契约与原型冻结（MCP + TUI 复用）

本文件冻结 M0 阶段的接口契约与原型范围，聚焦“先跑通最小闭环”，在保持 Codex 现有行为与 TUI 体验的前提下，为后续里程碑提供清晰边界。

---

## 范围（Scope）

- 明确 Codex 侧 MCP 工具契约（现有与拟新增），定义到 core 内部 Op 的语义映射。
- 定义 Bus（独立 MCP 服务）的工具契约（subscribe/publish/unsubscribe/control）。
- 给出调用样例、错误与超时约定、最小传输与跨机建议。
- 仅文档冻结，不改动现有代码（M1 起进入实现）。

---

## 一、Codex MCP 工具（M0 合同）

当前已实现（来自调查）：
- `codex`：新建会话并用首条用户输入驱动一次完整回合。
- `codex-reply`：向已存在会话提交下一条用户输入并执行到本轮结束。

拟统一命名（M1 将补齐，并保留后兼容）：
- `codex.reply`（等价现有 `codex-reply`）
- `codex.inject`（向“当前活跃任务”注入输入，不开启新回合）
- `codex.interrupt`（中断当前任务）
- `codex.shutdown`（结束会话）
- `codex.status`（查询会话状态）

M0 使用要求：
- 仅使用 `codex` 与 `codex-reply` 两个既有工具。
- Bus 的 `delivery` 模式在 M0 仅支持 `user_input`；`inject` 模式将在 M1 随 `codex.inject` 一同落地。

### 1.1 工具输入（M0）

- `codex` 入参（节选，kebab-case）：
  - `prompt: string`（必填）
  - `model?: string`，如 "o3"、"o4-mini"
  - `profile?: string`（config 档名）
  - `cwd?: string`（相对/绝对路径）
  - `approval-policy?: "untrusted"|"on-failure"|"on-request"|"never"`
  - `sandbox?: "read-only"|"workspace-write"|"danger-full-access"`
  - `config?: object`（JSON 值按 dotted‑path 转 TOML 覆盖）
  - `base-instructions?: string`
  - `include-plan-tool?: boolean`

- `codex-reply` 入参：
  - `sessionId: string`（必填，会话 UUID）
  - `prompt: string`（必填）

输出（两者一致）：
- 成功：`CallToolResult` 文本在 `content[0].text`（最后一条 agent 消息）
- 失败：`is_error: true` 且 `content[0].text` 为错误文本
- 运行过程中的审批通过 `elicitation.request` 交互（`exec-approval` / `patch-approval`）

参考实现文件：
- 列表与分发：`codex-rs/mcp-server/src/message_processor.rs`
- 工具 Schema：`codex-rs/mcp-server/src/codex_tool_config.rs`
- 运行逻辑：`codex-rs/mcp-server/src/codex_tool_runner.rs`
- 审批桥接：`codex-rs/mcp-server/src/exec_approval.rs`, `codex-rs/mcp-server/src/patch_approval.rs`

---

## 二、Bus MCP 工具（M0 契约）

Bus 只承载“Agent 间内容消息”与“最小控制面”。M0 支持 `user_input` 投递；`inject` 投递延至 M1。

### 2.1 工具清单

- `bus.subscribe`
- `bus.publish`
- `bus.unsubscribe`
- `bus.control`

### 2.2 输入/输出契约（JSON 约定，示意）

- `bus.subscribe`
  - params：
    - `topic: string`
    - `agent: { server: string, session_id?: string }`
      - 说明：`server` 为订阅者的 MCP 服务标识（codex 实例）；`session_id` 省略时可由 Bus 端策略创建新会话（M1+）。
    - `delivery?: "user_input"`（M0 固定为 `user_input`；`inject` 将在 M1 打开）
    - `ack_mode?: "none"|"manual"`（M0 固定 `none`）
  - result：`{ subscription_id: string }`

- `bus.publish`
  - params：
    - `topic: string`
    - `items: Array<any>`（默认将 `string` 映射为 Codex 的 `InputItem::Text { text }`）
    - `headers?: { correlation_id?: string, [k: string]: any }`
  - 语义：对所有订阅该 `topic` 的订阅者执行“反向调用”。M0 仅调用订阅者的 `codex-reply`（即 `user_input` 投递）。
  - result：`{ delivered: number }`

- `bus.unsubscribe`
  - params：`{ subscription_id: string }`
  - result：`{}`

- `bus.control`
  - params：`{ action: "start"|"interrupt"|"shutdown", agent?: { server: string, session_id?: string }, spec?: any }`
  - 语义：
    - `start`（M0 可选）：触发外部脚本/编排器启动子 Agent（或直接返回未实现），M1+ 具体化。
    - `interrupt`：对订阅者调用 `codex` 侧中断（M0 暂以“未实现”占位，M1 实现 `codex.interrupt` 后接入）。
    - `shutdown`：结束会话（同上，M1 实现后接入）。
  - result：`{ ok: boolean, detail?: string }`

### 2.3 错误与超时

- Bus 工具调用失败时：返回 `is_error: true` 与错误文本（包括订阅者端 MCP 调用错误）。
- 反向调用订阅者（`codex`/`codex-reply`）超时：M0 使用固定超时（例如 30s），重试最多 `N` 次（例如 2 次）；最终失败计数但不阻塞其他订阅者。
- 幂等：建议调用方提供 `headers.correlation_id`；Bus 可做去重缓存（M1+）。

---

## 三、映射关系与投递语义（M0）

- `delivery = user_input`（唯一支持）：Bus → 订阅者执行 `codex-reply`，把 `items` 转为 `prompt` 文本；订阅者进入新一回合处理。
- `delivery = inject`（M1 开启）：Bus → 订阅者执行 `codex.inject`，在活跃任务流内注入输入；若无活跃任务可配置降级为 `user_input` 或拒绝。

---

## 四、调用样例（示意）

- 订阅（观察者或子 Agent 订阅某主题 inbox）
```json
{
  "type": "request",
  "method": "tools/call",
  "params": {
    "name": "bus.subscribe",
    "arguments": {
      "topic": "agent/reviewer/inbox",
      "agent": { "server": "codex_reviewer" },
      "delivery": "user_input"
    }
  }
}
```

- 发布（向 inbox 发布文本消息）
```json
{
  "type": "request",
  "method": "tools/call",
  "params": {
    "name": "bus.publish",
    "arguments": {
      "topic": "agent/reviewer/inbox",
      "items": ["请审核 PR#421"],
      "headers": { "correlation_id": "c-2025-09-03-0001" }
    }
  }
}
```

- 继续会话（订阅者侧 Codex，供参考）
```json
{
  "type": "request",
  "method": "tools/call",
  "params": {
    "name": "codex-reply",
    "arguments": {
      "sessionId": "6f5d0a74-6a2d-4c4a-8b9d-3e0d5d5a1a42",
      "prompt": "请审核 PR#421"
    }
  }
}
```

---

## 五、最小传输与跨机建议（M0）

- 传输：沿用当前实现（stdio）。跨机建议优先使用 SSH，把远端 `codex-mcp-server`/`codex-bus` 的 stdio 映射到本地。
- 配置：在 `~/.codex/config.toml` 的 `mcp_servers.*` 中将 `command` 写为 `ssh`，`args` 指定远端命令与参数。

---

## 六、兼容性与迁移

- M0 仅引入文档契约，不破坏现有 `codex`/`codex-reply` 行为。
- M1 起：
  - 新增 `codex.reply`/`codex.inject`/`codex.interrupt`/`codex.shutdown`/`codex.status`，并保留 `codex`/`codex-reply` 后兼容。
  - Bus 打开 `delivery=inject`，并接入中断/结束控制。

---

## 七、验收清单（M0）

- [ ] Bus 工具契约冻结（本文）
- [ ] Codex 工具统一命名与新增项达成一致（文档层面）
- [ ] 端到端最小调用样例可用于联调（文档/脚本）
- [ ] 跨机方案（SSH）达成一致（文档层面）

> 说明：M0 只做“定义与对齐”，不改动代码；M1 起进入实现与验证。

