# Codex Core 在批处理与强化学习场景下的设计与集成报告

本文面向开发人员，系统性介绍 codex core 的能力边界、与 TUI 的交互机制；给出“面向数据集批量运行”的方案设计与实现样例；并结合强化学习（RL）训练对 logprobs 等指标的诉求，提出在 codex core 中的改造路径与下一步建议。

---

## 1. 架构概览：Codex Core 职责与边界

- 定位：`codex-core` 是业务逻辑与模型/工具编排层，不负责 UI 渲染。
- 通信模型：严格的 SQ/EQ（Submission Queue / Event Queue）模式。
  - 入队：`protocol::Op`（例如 `UserInput`, `Interrupt`, `GetHistory` 等）。
  - 出队：`protocol::EventMsg`（例如 `AgentMessageDelta`, `ExecCommandBegin/End`, `TokenCount`, `TaskComplete` 等）。
- 关键对象
  - `Codex`：单会话的“收发器”，内部维护提交/事件通道；`Codex::spawn()` 启动 `submission_loop`。
  - `Session` + `TurnContext`：会话状态与一次回合的上下文（模型、策略、工具、cwd 等）。
  - `ConversationManager`：管理多会话（`HashMap<Uuid, Arc<CodexConversation>>`），可并发创建/持有多条独立对话。
  - `ModelClient`：对接上游模型流（OpenAI Responses/Chat）并统一适配为 `ResponseEvent`。
  - 工具执行：`handle_function_call` → `run_exec_with_events` 发出 `Exec*`/`Patch*` 事件，并聚合 stdout/stderr/格式化输出。
  - 轨迹持久化：`RolloutRecorder` 将对话项（`ResponseItem`）写入 `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`。

### 1.1 事件驱动链路（简图）

```
UI/调用方 ──(Op)──> Codex(提交队列) ──▶ submission_loop
                                     └─▶ run_task (多轮回合)
                                         └─▶ run_turn (断线重试)
                                             └─▶ try_run_turn (消费 ResponseEvent 流)
                                                 ├─ OutputTextDelta → AgentMessageDelta
                                                 ├─ Reasoning*Delta → AgentReasoning*(…)
                                                 ├─ OutputItemDone(item) → handle_response_item
                                                 │    ├─ Message → AgentMessage
                                                 │    └─ Function/LocalShell/Custom → 执行工具 → Exec*/Patch* 事件
                                                 └─ Completed{token_usage} → TokenCount, TurnDiff, 返回回合结果
```

- 注意：核心从不直接打印；所有面向用户的反馈均通过 `EventMsg` 事件交由上层（如 TUI/CLI）消费。

### 1.2 与 TUI 的关系

- TUI 仅是 `EventMsg` 的消费者与 `Op` 的生产者；UI 无状态、可替换。
- `protocol` crate 定义的 `Op`/`EventMsg` 是唯一耦合面；`codex-core` 与 TUI 渲染无耦合。
- 这意味着：可以完全绕过 TUI，直接以库方式调用 core 来批量/无 UI 运行，并从事件流或 rollout 文件获取原始轨迹。

---

## 2. 面向数据集的批量运行方案

目标：从自动生成的数据集读取多条样本（prompt），为每条样本创建独立会话并执行一轮或多轮推理，收集原始 trajectory（文本增量、工具调用、token 用量、最终对话历史等），不依赖 TUI。

### 2.1 模块设计

- `ConfigFactory`
  - 负责生成 `core::config::Config`，通过 `ConfigOverrides` 定制模型、审批策略（建议 `AskForApproval::Never`）、沙箱策略等。
  - 可通过 `CODEX_HOME` 指向专用目录，集中存放 rollout。

- `DatasetLoader`
  - 从文件/数据库/内存提供样本集合：`Vec<Sample> { id, text, … }`。

- `Runner`（核心编排）
  - 持有 `ConversationManager`，为每个样本创建会话（`new_conversation`）。
  - 将样本文本封装为 `Op::UserInput { items: vec![InputItem::Text{…}] }` 提交。
  - 并发消费每个会话的 `next_event()`，直到 `EventMsg::TaskComplete`。
  - 将事件实时写入样本结果（本地 JSONL 或内存结构）。

- `EventCollector`
  - 订阅并筛选有价值的事件：`AgentMessageDelta/AgentMessage`、`TokenCount`、`Exec*`、`TurnDiff`、`StreamError` 等。
  - 可选：在回合结束后发 `Op::GetHistory` 获取完整 `ConversationHistory(entries)`。

- `OutputWriter`
  - 统一落盘策略（例如：每样本一份 JSONL 或 Parquet；或直接使用内置 rollout 文件）。

- `ConcurrencyController`
  - 基于 `tokio::sync::Semaphore` 限制并发度，处理速率限制与退避（codex-core 内部已对 SSE 断线做重试）。

### 2.2 关键接口与数据流

- 创建会话
  - `ConversationManager::new_conversation(config)` → `NewConversation { conversation_id, conversation, session_configured }`
- 发送输入
  - `CodexConversation::submit(Op::UserInput { items })` → 提交 id
- 读取事件
  - `CodexConversation::next_event()`（循环直到 `TaskComplete`）
- 获取历史（可选）
  - `submit(Op::GetHistory)` → 读取 `EventMsg::ConversationHistory(ConversationHistoryResponseEvent)`

### 2.3 实现样例（最小可用 Rust）

> 说明：示例强调核心调用路径，错误处理与资源清理从略；真实项目请补充健壮性与并发控制。

```rust
use codex_core::ConversationManager;
use codex_core::config::{Config, ConfigOverrides};
use codex_core::protocol::{Op, InputItem, EventMsg};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1) 构造 Config（示例：禁止审批、工作目录设为当前项目）
    let cfg = Config::load_with_cli_overrides(
        vec![],
        ConfigOverrides {
            // 根据需要覆盖：模型、沙箱、策略等
            // model: Some("gpt-4o-mini".into()),
            // approval_policy: Some(codex_core::protocol::AskForApproval::Never),
            // sandbox_mode: Some(codex_core::protocol_config_types::SandboxMode::ReadOnly),
            ..Default::default()
        },
    )?;

    // 2) 多会话管理器
    let cm = ConversationManager::new(codex_login::AuthManager::default());

    // 3) 读取数据集（示例用内存）
    let dataset = vec![
        ("sample-1", "请总结 README.md 的要点"),
        ("sample-2", "解释 core/codex.rs 中 run_task 的控制流"),
    ];

    // 4) 顺序跑（并行请使用 tokio::spawn + Semaphore）
    for (sid, prompt) in dataset {
        let new_conv = cm.new_conversation(cfg.clone()).await?;
        let conv = new_conv.conversation;

        // 提交用户输入
        conv
            .submit(Op::UserInput {
                items: vec![InputItem::Text { text: prompt.to_string() }],
            })
            .await?;

        // 事件消费直到 TaskComplete
        loop {
            let ev = conv.next_event().await?;
            match ev.msg {
                EventMsg::AgentMessageDelta(d) => {
                    // 增量文本（可写入你自己的 JSONL）
                    println!("DELTA: {}", d.delta);
                }
                EventMsg::AgentMessage(m) => {
                    println!("FINAL: {}", m.message);
                }
                EventMsg::TokenCount(t) => {
                    println!("TOKENS: total={}, output={}", t.total_tokens, t.output_tokens);
                }
                EventMsg::TaskComplete(_) => break,
                _ => {}
            }
        }

        // 可选：获取内存中的完整会话历史
        conv.submit(Op::GetHistory).await?;
        let history = conv.next_event().await?; // 期待 ConversationHistory
        if let EventMsg::ConversationHistory(h) = history.msg {
            println!("{} history entries", h.entries.len());
        }
    }

    Ok(())
}
```

### 2.4 实战建议

- 审批策略：批处理环境使用 `AskForApproval::Never`，避免交互阻塞；必要时限制沙箱为 `ReadOnly` 或 `WorkspaceWrite`。
- 工具开关：非必须时关闭 `include_apply_patch_tool`、`tools_web_search_request`、`include_view_image_tool`，降低非确定性与事件噪音。
- 轨迹归档：
  - 需要“完整回放”→ 直接消费 `~/.codex/sessions/.../rollout-*.jsonl`。
  - 需要“按需裁剪的数据集”→ 自行订阅事件并落地 JSONL/Parquet。
- 并发与限流：结合上游限速策略，使用 `Semaphore` 控制同时活跃会话数；codex-core 已对 SSE 断线提供退避重试。

---

## 3. 强化学习（RL）所需的 logprobs 与扩展设计

### 3.1 现状评估

- 当前输出粒度：
  - 文本增量：`EventMsg::AgentMessageDelta`（字符串片段，不保证 tokenizer 边界）。
  - 推理增量：`AgentReasoningDelta`、`AgentReasoningRawContentDelta`（可开关）。
  - 用量统计：`TokenCount(TokenUsage)`（Responses API 路径可得，Chat Completions 路径无用量）。
- 不具备：
  - token 级边界与 id。
  - token 级 `logprobs`/`top_logprobs`。
  - rollout 持久化中也未记录上述指标。

结论：要满足 RL 训练的数据需求，需要对 core 在“请求构造 → SSE 解析 → 事件建模 → 可选持久化”四处进行增强。

### 3.2 能力扩展方案（建议）

1) 协议与事件层（protocol + core 公共事件）
- 在 `core/src/client_common.rs` 增加：
  - `ResponseEvent::TokenLogprobsDelta { token: String, logprob: f32, top: Vec<(String, f32)> }`
- 在 `protocol/src/protocol.rs` 增加：
  - `EventMsg::TokenLogprobsDelta(TokenLogprobsDeltaEvent)` 与载荷结构：
    ```rust
    pub struct TokenLogprobsDeltaEvent {
        pub token: String,
        pub logprob: f32,
        pub top: Vec<(String, f32)>,
    }
    ```
- 在 `core/src/codex.rs::try_run_turn` 中转发新的 `ResponseEvent::TokenLogprobsDelta` 为 `EventMsg::TokenLogprobsDelta`。

2) 请求构造与能力开关
- `Config`/`ConfigOverrides` 新增：
  - `collect_logprobs: Option<bool>`、`top_logprobs: Option<u32>`。
- Responses API 路径（`core/src/client.rs::stream_responses`）：
  - 在 `ResponsesApiRequest` 中加入可选字段或 `include` 项以开启 logprobs（需根据提供方支持情况调整，保持向后兼容）。
- Chat Completions 路径（`core/src/chat_completions.rs`）：
  - 请求 JSON 增加 `logprobs` 与 `top_logprobs`（上游是否支持需按模型家族判断）。

3) SSE 解析
- Responses API：扩展 `SseEvent` 结构或保留 `serde_json::Value` 直接读取 `logprobs` 字段；在 `process_sse` 中将 token 级数据映射为 `ResponseEvent::TokenLogprobsDelta`。
- Chat Completions：在 `process_chat_sse` 中解析 `choices[].delta.logprobs`（如存在），逐 token 发送对应事件。

4) 持久化（可选）
- `rollout.rs`：为 token 级指标新增专属记录（例如：
  ```json
  {"record_type":"token_logprobs","token":"…","logprob":-1.23,"top":[["…",-1.0],…]}
  ```
  ）；或单独输出为外部 JSONL。

5) 模型/提供方能力描述
- 在 `ModelFamily`/`ModelProviderInfo` 中新增能力标志（是否支持 logprobs、字段名/形态），避免对不支持的路径发送无效参数。

### 3.3 迭代落地建议

- 第 1 阶段（POC）
  - 在 Chat Completions 或 Responses 任选一路打通 logprobs（取决于你的目标模型支持度）。
  - 仅在内存事件流中暴露 `TokenLogprobsDelta`，不改动 rollout。
- 第 2 阶段（协议固化）
  - 将事件类型稳定到 `protocol`，并在 `Config` 增加显式开关。
  - 引入 provider 能力检测与自动降级（不开启或降为仅文本 delta）。
- 第 3 阶段（数据工程）
  - 设计并固化 RL 训练所需的样本格式（对齐 tokenizer 与 token id），统一落盘与回放工具。
- 第 4 阶段（质量保障）
  - 基于 SSE fixture 的端到端测试：覆盖“正常 / 断线重试 / 无 logprobs 降级”等分支。

---

## 4. 操作注意事项与最佳实践

- 安全与审批：批量任务默认不应升级到人工审批；如确需写磁盘，优先在受控目录与受控工具集内进行。
- 并发控制：避免因上游限流导致整体失败；必要时基于样本维度做指数退避与重试。
- 事件归并：训练侧通常希望保留“原始增量”而非聚合文本；在 Chat 路径中可通过配置启用“流式聚合模式”但同时转发 delta。
- 可观测性：利用 `StreamError` 与 `BackgroundEvent` 事件，将断线与退避信息写入日志，便于排障。

---

## 5. 结论

- 解耦性：codex core 与 TUI 的耦合面仅在 `protocol::{Op, EventMsg}`，可稳定支撑“无 UI 的批处理/数据集驱动执行”。
- 轨迹：可直接使用事件流或内置 rollout 文件；如需自定义数据格式，建议在 Runner 中即时转存。
- RL 扩展：当前不含 token 级 `logprobs`；按本文方案对请求、解析、事件、配置进行小范围增强后，即可满足 RL 训练的指标采集需求。

---

### 附：常用事件一览（节选）

- 文本/推理：`AgentMessageDelta`, `AgentMessage`, `AgentReasoningDelta`, `AgentReasoningRawContentDelta`, `AgentReasoningSectionBreak`
- 工具：`ExecCommandBegin/OutputDelta/End`, `ApplyPatchApprovalRequest`, `PatchApplyBegin/End`
- 其他：`TokenCount`, `TurnDiff`, `WebSearchBegin/End`, `StreamError`, `TaskStarted`, `TaskComplete`

