# Codex 多 Agent 消息总线方案（MCP + TUI 复用）

本文将本次讨论结果系统化，给出一个可落地、易扩展的多 Agent 架构，最大程度复用 Codex 现有能力（core/TUI/MCP）。

- 1. 整体架构详解
- 2. 消息总线（Bus）方案详解：能力、接口、实现方式
- 3. Codex 需要支持的能力与最小改造点
- 4. 可扩展性（跨机器、伸缩、可靠性）
- 5. 其他可选方案对比与取舍

---

## 1. 整体架构详解

目标：在保持“自然交互（对话/工具/审批/Diff）”与“通用通信模式（Pub/Sub）”的同时，保留人类熟悉的原生 TUI 调试体验，并支持跨机器伸缩。

- 角色
  - Bus（独立模块/MCP 服务器）：只承载“Agent 间消息内容”的发布/订阅，以及最小控制面（启动/中断/结束）。不扩散其他 Agent 的内部事件。
  - 子 Agent（Codex 会话）：推荐以“无头 MCP 服务器”运行（需要时可开启 TUI 版本调试）。每个子 Agent 订阅自身的 inbox 主题，接收来自 Bus 的消息；也可通过 `mcp_servers` 调用 Bus 或其他 MCP 服务。
  - 观察者 TUI（可选）：一个普通的 Codex + TUI 会话，通过 MCP 工具向 Bus 发起 `/subscribe`，把某个 topic 的消息作为“标准对话输入”接收与展示；从而实现人类参与与调试。

- 消息流（简化）：
  1) 发布者（Agent/观察者）→ Bus.publish(topic, items)
  2) Bus 查询订阅表，按订阅的 delivery 模式对订阅者执行“反向调用”：
     - 有活跃任务 → 调订阅者的 codex.inject（等价 `sess.inject_input`）
     - 无活跃任务 → 调订阅者的 codex.reply（等价 `Op::UserInput`，启动新任务）
  3) 订阅者会话把消息当作普通输入进入 turn 循环，走现有工具/审批/渲染路径。
  4) 如需控制，中断或关闭订阅者：Bus → codex.interrupt/codex.shutdown。

- 人类观察：
  - 不需要“事件镜像”。观察者只需订阅业务 topic，消息即以对话形式呈现，原生 TUI 组件（聊天、Diff、审批、日志）全复用。

---

## 2. 消息总线（Bus）方案详解

### 2.1 能力清单

- 内容通道（核心）
  - subscribe(topic, agent, delivery)：把“订阅者会话”登记在某主题上；delivery 决定投递到订阅者后的处理方式
  - publish(topic, items, headers)：发布消息（文本/附件引用/元数据）；Bus 负责把消息推送到所有订阅者
  - unsubscribe(subscription_id)：取消订阅
  - 可选：poll/ack/死信队列（大规模异步时使用）

- 控制通道（最小集）
  - control.start(spec)：启动子 Agent（调用外部脚本/编排器/Factory），创建会话并可选发起初始订阅
  - control.interrupt(agent_id)：急停当前任务（等价目标会话 `Op::Interrupt`）
  - control.shutdown(agent_id)：结束会话（等价 `Op::Shutdown`）

- 安全/治理（建议）
  - ACL（谁能订阅/发布/控制谁）；速率/并发/配额
  - 统一 correlation_id/trace_id，便于问题定位

### 2.2 MCP 工具接口（草案）

以 JSON-Schema 风格描述（示意）：

```jsonc
// 内容
bus.subscribe: {
  params: { topic: string, agent: { server: string, session_id?: string }, delivery?: "inject"|"user_input", ack_mode?: "none"|"manual" },
  result: { subscription_id: string }
}

bus.publish: {
  params: { topic: string, items: Array<any>, headers?: { correlation_id?: string, [k: string]: any } },
  result: { delivered: number }
}

bus.unsubscribe: {
  params: { subscription_id: string },
  result: {}
}

// 控制
bus.control: {
  params: { action: "start"|"interrupt"|"shutdown", agent?: { server: string, session_id?: string }, spec?: any },
  result: { ok: boolean, detail?: string }
}
```

- delivery 语义：
  - inject：并入订阅者当前任务（长跑/交互式）；无活跃任务时可降级为 user_input 或拒绝（由 Bus 策略决定）
  - user_input：每条消息触发订阅者的新回合（离散任务友好）

- items 映射：
  - 默认文本映射到 Codex 的 `InputItem::Text { text }`
  - 大对象建议传“引用”（artifact URL/ID）而非内联字节

### 2.3 投递实现

- 反向调用订阅者的 Codex MCP 端点：
  - inject → 订阅者工具 `codex.inject`（等价 `sess.inject_input`）
  - user_input → 订阅者工具 `codex.reply`（等价 `Op::UserInput`）
  - interrupt/shutdown → 订阅者工具 `codex.interrupt` / `codex.shutdown`

- 跨机器传输：
  - 先行路径：用 SSH 把 MCP 的 stdio 透传到远程进程（零改代码）
  - 工程化路径：加“mcp‑net‑gateway”（stdio ↔ TCP/WS 网关）或扩展 mcp-client/server 直连 TCP/WS

- 可靠性（可选）
  - 超时/重试：对单一订阅者的推送失败→重试；多次失败→死信队列
  - 顺序：在“主题分区”内提供单分区顺序；跨分区不保证全局顺序
  - 语义：以“至少一次”为主；要求消息具幂等性（用 correlation_id 去重）

---

## 3. Codex 需要支持的能力与最小改造

- 子 Agent（Codex 会话）
  - 以“无头 MCP 服务器”运行：已具备（`codex-rs/mcp-server`）
  - 暴露工具：`codex.reply`（UserInput）、`codex.inject`（注入输入）、`codex.interrupt`、`codex.shutdown`、`codex.status`
  - 会话内可访问 Bus/其他 MCP：已具备（`McpConnectionManager::new(config.mcp_servers)`），模型也可直接调用 Bus 工具

- 观察者 TUI（可选增强，非必需）
  - 两条便捷命令（在人类操控场景更丝滑）：
    - `/subscribe <topic> [inject|user_input]` → 调 Bus.subscribe，把“当前观察者会话”加到主题
    - `/publish <topic> <text|payload>` → 调 Bus.publish，给目标主题发消息
  - 这两条命令只是把“Bus 操作”以人类可用形式暴露；收到的消息仍走 TUI 原生渲染，无需重写 UI 组件

- 维持“零侵入”的关键点
  - 不需要镜像其他 Agent 的事件到 Bus；观察者看到的是“内容消息”，以对话方式展现
  - 审批/工具/差异/日志一切留在各自会话内部，TUI 全量复用

---

## 4. 可扩展性（跨机器、伸缩、可靠性）

- 跨机器/跨网络
  - 方式一（最快）：SSH 远程拉起 MCP 进程（`command=ssh ... args=[remote, "codex-mcp-server"]`），MCP 帧天然穿透
  - 方式二：网关/Sidecar（stdio ↔ TCP/WS），统一 TLS/mTLS、限流与健康检查
  - 方式三：扩展 mcp-client/server 支持 TCP/WS（需要少量代码）

- 伸缩与分片
  - topic 分区（hash(key) → partition）横向扩容；订阅方按分区绑定副本
  - Bus 无状态层 + 存储层（可选）支撑回放/死信；前端多副本 + 反压策略

- 可靠性与治理
  - 语义：至少一次；要求 producer/consumer 幂等
  - 超时/重试/死信：对单个订阅者失败的推送重试 N 次后转入 DLQ
  - 度量：投递延迟、重试次数、队列深度、吞吐；统一 trace/correlation_id
  - ACL：topic‑level 与 control‑level 权限，防止越权控制

---

## 5. 其他可选方案对比

下列方案均可实现多 Agent，但与“Bus 只承载内容消息 + 原生 TUI 复用”的目标相比，各有取舍：

- A. MCP Hub/Router（集中转发工具调用）
  - 思路：Hub 把各 Agent 暴露的工具聚合再导出，调用方通过 Hub 间接调用别人
  - 优点：统一路由/ACL/审计；任意拓扑（逻辑）
  - 缺点：交互以“函数调用”为主，不天然支持“对话式内容消息”；与“人类对话视图”融合度低于 Bus

- B. 事件镜像方案（Bus 镜像各 Agent 的 Codex 事件）
  - 优点：单接入点、统一回放与指标；观察者只连 Bus
  - 缺点：与“只传业务内容消息”的设定不符；实现与治理复杂度高；非必要

- C. 观察者 TUI 直连多 Agent（无 Bus 镜像）
  - 优点：简单直接，少一跳
  - 缺点：TUI 要管理多连接/排序/存储；多观察者时 N 倍放大；跨网络接入复杂

- D. Session Bridge（在单进程内把各会话工具再导出）
  - 优点：进程内低开销互调
  - 缺点：仅限同进程，跨机器与解耦不足

- E. 仅星型（主 Agent ↔ 子 Agent）
  - 优点：实现最简单
  - 缺点：通信模式受限；不利于演进与解耦

综合对比：本方案（Bus 仅承载内容消息 + 最小控制 + 原生 TUI 复用）在“人类协作范式、系统解耦、实施成本、演进空间”之间取得更好平衡。

---

## 集成步骤（速查）

1) 部署 Bus（MCP 服务器）。实现 bus.subscribe/publish/control，内部以“codex.reply/inject/interrupt/shutdown”反向调用订阅者。
2) 子 Agent 以“无头” Codex MCP 服务器运行；启动时订阅自身 inbox（如 `agent/<id>/inbox`）。
3) 观察者：启动普通 Codex + TUI，会话中执行一次订阅（可通过模型/或 slash 命令增强）。
4) 发布消息：任何一端调用 bus.publish(topic, items)；Bus 按订阅表推送。
5) 控制：需要时由 Bus 发起 interrupt/shutdown；或启动新会话（start）。

> 附：跨机器优先用 SSH 映射 stdio；有长期托管/公网需求再补充网关或增强 mcp‑client/server。

