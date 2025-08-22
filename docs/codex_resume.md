# Codex Resume/Branch 功能设计与实现进度（Rust 版 CLI/TUI）

本文档汇总了“resume/回退/分叉（branch）”功能的设计、落地实现、用法、当前进度与下一步计划，便于后续团队持续推进与追踪。

## 背景与目标

- 背景：Codex CLI/TUI 默认会把一个会话（session）的交互写入本地 `~/.codex/sessions/**/rollout-*.jsonl`。用户希望：
  - 继续（resume）既有会话，或回退到某次历史交互从此处“分叉”。
  - 复用既有上下文，避免把历史整段再发给模型（节省 tokens/加速），也避免在 JSONL 里重复写历史内容。
- 目标：
  - 支持两条路径：
    1) 服务端“链式续接”（Responses API `previous_response_id`）。
    2) 本地“重放”（rollout resume，本地 UI 展示历史，不把旧上下文重复发送）。
  - CLI 与 TUI 都要提供良好操作体验；TUI 内 resume 后，应直接把历史上下文渲染在 UI，用户直接继续输入新的 prompt。

## 方案概述

- 服务端续接（首选）：
  - 记录每次 turn 的 `response_id`；下次请求在 `store: true` 时携带 `previous_response_id`（即上次的 `response_id`），服务端自动复用上下文。
  - 从任意历史节点“分叉”：把 `previous_response_id` 设置为目标历史节点的 `response_id` 即可。
- 本地重放：
  - 仍然使用现有的 rollout resume 机制，让 TUI/CLI 从 JSONL 解析历史消息并展现；同时在能 `store: true` 的场景下配合 `previous_response_id`，避免重复发送旧上下文。

## 关键实现

### 核心 core（codex-rs/core）

- 新增/修改的数据结构与请求参数：
  - `client_common::Prompt` 新增 `previous_response_id: Option<String>`。
  - `client_common::ResponsesApiRequest` 新增 `previous_response_id` 字段（仅在 `store: true` 时发送）。
- 记录与恢复“续接锚点”：
  - 每次 turn 完成（收到 `response.completed`）后：
    - 把 `response_id` 写入 session 状态 `last_response_id`；
    - 写入 rollout 状态行（JSONL 第一层对象，`{"record_type":"state", ...}`），内容包括：
      - `last_response_id: Option<String>`
      - `created_at: Option<String>`（RFC3339，UTC）
      - `summary: Option<String>`（助手消息内容的短摘要，截断 200 字符）
  - `rollout::SessionStateSnapshot` 结构扩展为上述 3 字段；`resume()` 时可恢复状态。
- 自动携带 `previous_response_id`：
  - 构造请求 `store: true` 且有缓存的 `last_response_id` 时，自动塞到 `Prompt.previous_response_id`，进而序列化到 `ResponsesApiRequest.previous_response_id`。
- 配置项（实验用）：
  - `Config.experimental_resume: Option<PathBuf>`：指定首帧从某个 rollout 文件恢复（现有功能）。
  - 新增 `Config.experimental_previous_response_id: Option<String>`：允许首帧强制指定 `previous_response_id`，用于从历史节点“分叉”首个 turn。

### CLI（codex-rs/cli）

- 新增 `codex sessions` 子命令：
  - `list`：列出 `~/.codex/sessions/**/rollout-*.jsonl`（`session_id  path`）。
  - `show <SID|PATH>`：解析 rollout 中的 state 行，输出“step 序号 + resp 短 id + 时间 + 摘要”。
    - 老的 rollout 文件若无 `created_at/summary`，会退化为只显示 `resp`。
  - `resume <SID|PATH> --prompt "..." [--at RESP_ID] [--step N]`：
    - 注入 `-c experimental_resume="..."`。
    - `--at` 直接指定 `previous_response_id`；`--step` 自动把第 N 个 step 映射为 `previous_response_id`。
    - 在 `store: true` 场景下，不再重复发历史上下文。
  - `branch <SID|PATH> --from RESP_ID --name BRANCH` / `checkout <SID|PATH> --branch BRANCH`
    - 在 session 目录写入/更新 `resume-index.json` 分支指针（基础的轻量逻辑指针，后续可扩展）。

### TUI（codex-rs/tui）

- Slash 命令（已接入）：
  - `/resume <SID|PATH> [--at RESP_ID | --step N]`
    - 不再要求 prompt；TUI 会：
      1) 使用 `experimental_resume` + 可选 `experimental_previous_response_id` 启动新会话；
      2) 解析 rollout 并把历史“用户/助手”消息渲染到 UI；
      3) 输入框保持为空，等待用户继续输入；
  - `/branch <SID|PATH> --from RESP_ID --name BRANCH`：写入/更新 `resume-index.json`。
- 关键实现细节：
  - ChatWidget: 解析 `/resume`、`/branch` 并向 App 层发送 `AppEvent::ResumeRequest`/`BranchRequest`。
  - App: 
    - `try_resume_chat()`：根据 target 解析到 rollout 路径，按 `--at/--step` 得到 `previous_response_id`，设置到新 `Config`，创建新的 `ChatWidget`，然后调用 `replay_rollout_to_history()` 将历史消息渲染到 UI；
    - `try_branch()`：就地更新 `resume-index.json`。

## 用法示例

### CLI

```bash
# 列出会话
cargo run -p codex-cli -- sessions list

# 查看某会话的时间线（resp + 时间 + 摘要）
cargo run -p codex-cli -- sessions show <SESSION_ID>

# 从第 N 个节点“分叉”并 resume（不重复发历史上下文）
cargo run -p codex-cli -- sessions resume <SESSION_ID> --step 2 --prompt "继续这里"

# 直接指定 response_id resume
cargo run -p codex-cli -- sessions resume <SESSION_ID> --at resp_xxx --prompt "继续这里"

# 创建分支指针
cargo run -p codex-cli -- sessions branch <SESSION_ID> --from resp_xxx --name bugfix-1

# 切换分支指针
cargo run -p codex-cli -- sessions checkout <SESSION_ID> --branch bugfix-1
```

### TUI

- 在输入框键入：

```text
/resume <SESSION_ID|/full/path/to/rollout.jsonl> [--at RESP_ID | --step N]
```

- 行为：UI 立即展示该会话此前的“用户/助手”上下文，输入框为空，用户直接继续输入；内部请求在 `store: true` 时会携带 `previous_response_id`，复用服务端上下文，不会把历史重复发给模型。

- 分支：

```text
/branch <SESSION_ID|PATH> --from RESP_ID --name BRANCH
```

## 注意事项与边界

- `previous_response_id` 仅在 `store: true` 时对 Responses API 生效：
  - ChatGPT Auth（ZDR 方案）下我们会禁用 `store`，这时会 fallback 为“重放 + 全量上下文发送”。
- 老的 rollout 文件没有 `created_at/summary`；`sessions show` 会退化为只输出 resp 短 id。
- SSE Fixture（测试夹具）当前只有一个 `response.completed`（resp1），因此 `sessions show` 在该夹具下只显示一个 step；真实多回合会话会正常显示多步。
- 新增依赖：
  - `cli/tui`：`chrono`、`rand`（用于元数据与简易 id）。

## 进度与提交

- 分支：`feat/codex-resume-cli`
- 提交摘要：
  1) `cli: add codex sessions subcommand with list/resume (MVP) using experimental rollout resume`
  2) `core: add previous_response_id resume support and record last_response_id in rollout state; cli: sessions show/branch/checkout and --at <response_id> to resume via server chaining`
  3) `cli: sessions show now prints time/summary; sessions resume supports --step; core: record created_at/summary in rollout state; tui: add /resume and /branch commands to resume/branch within TUI`
  4) `tui: adjust /resume to not require prompt; replay rollout transcript (user/assistant messages) into UI; add /branch + /resume requests; show composer empty after resume`

## 当前状态（与已满足的诉求）

- 复用历史上下文：核心已支持 `previous_response_id`，在 `store: true` 下不再重复发送历史；同时 JSONL 不重复写入旧内容。
- 按 step resume：CLI/TUI 都已支持（`--step`）。
- TUI 不再要求在 `/resume` 中输入 prompt，resume 时直接在 UI 中展示此前上下文，用户继续输入即可。
- CLI `sessions show` 已展示时间与摘要（针对新生成的会话）。

## 下一步计划

- TUI 时间线选择器：
  - 新增一个时间线视图（或在 `/resume` 后弹出）列出 step 列表（时间/摘要），支持选择 step 直接 resume 或“创建分支并 resume”。
- CLI `sessions branch` 补充 `--step N` 参数（目前为 `--from RESP_ID`）。
- 多回合验证：
  - 增加多回合 fixture 或集成测试，确保 `show` 能展示多步；完善 `/resume --step` 选择逻辑的测试覆盖。
- 文档与帮助信息：
  - `--help` 与 README 里补充新的子命令与参数说明；TUI 内新手引导提示 resume/branch 用法。

## 关键接口与触点（便于代码走查）

- 核心：
  - `core/src/client_common.rs` → `Prompt { previous_response_id }`，`ResponsesApiRequest { previous_response_id }`
  - `core/src/codex.rs` → turn 完成时记录 `last_response_id`，`record_completed_state()` 写入 state（`created_at`、`summary`）。
  - `core/src/rollout.rs` → `SessionStateSnapshot { last_response_id, created_at, summary }`。
  - `core/src/config.rs` → `experimental_previous_response_id`；`experimental_resume` 仍保留。
- CLI：
  - `cli/src/sessions.rs` → `list`/`show`/`resume --at/--step`/`branch`/`checkout` 实现。
- TUI：
  - `tui/src/chatwidget.rs` → 解析 `/resume`、`/branch` 并向 App 发送事件。
  - `tui/src/app.rs` → `try_resume_chat()`、`replay_rollout_to_history()`、`try_branch()`。
  - `tui/src/history_cell.rs` → 新增 `new_assistant_message()`，用于回放历史。

## 风险与回滚

- 若后续需要临时禁用 `previous_response_id`（兼容性排查），可在 CLI/TUI 侧先不传 `--at/--step`，在 `store=false` 场景不产生影响。
- `rollout` 的 state 行格式向后兼容（老文件仅缺少新字段）。

---

如需任何额外信息或深入代码走查，请在此文档补充“问题记录”小节并 @相关开发者即可。
