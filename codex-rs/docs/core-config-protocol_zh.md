# Codex 配置与协议上下文详解（core 专注）

> 本文聚焦 `codex-rs/core` 中“配置加载/合并、模型提供方、沙箱与 Shell 环境策略、环境上下文注入”及 `codex-protocol` 的事件/数据模型，帮助你准确理解上下文如何被构建并在端到端交互中传递。

---

## 一、配置系统概览（core/src/config*.rs, flags.rs）

### 加载与优先级
- 入口：`Config::load_with_cli_overrides(cli_overrides, overrides)`
  - 步骤：
    1) 解析 `CODEX_HOME/config.toml` 为 `TomlValue`（无文件→空表）。
    2) 应用 `-c key=value` 样式的“通用 CLI 覆盖”（dotted-path），修改 `TomlValue`。
    3) 反序列化为强类型 `ConfigToml`。
    4) 应用强类型 `ConfigOverrides`（来自命令行参数或调用侧）。
  - 最终优先级：`config.toml` < `-c` 覆盖 < `ConfigOverrides`。
- `ConfigToml` → `Config` 的合并细节：
  - 模型与族：若无法从模型 slug 推断 `ModelFamily`，回退到临时构造（包含是否支持 reasoning summaries 等标志）。
  - 模型上下文窗口/最大输出：优先 `ConfigToml`，否则从 `openai_model_info` 查。
  - `responses_originator_header`：默认为 `codex_cli_rs`，可被内部覆盖项替换。
  - `notify`：外部每个“回合完成”触发的命令（argv 形式，不包含 JSON 载荷；Codex 追加 JSON）。
  - `history`、`file_opener`（URI 方案）、`tui` 子配置、`project_doc_max_bytes`（截断 AGENTS.md 嵌入文档）。
  - `tools`：`web_search` 与 `view_image` 开关（默认 `view_image=true`）。
  - `preferred_auth_method`、`chatgpt_base_url` 等。

### Sandbox 与审批策略
- `SandboxMode`（protocol）与 `SandboxPolicy`（protocol）映射：
  - DangerFullAccess / ReadOnly / WorkspaceWrite（可配置 `writable_roots`、`network_access`、`exclude_tmpdir_env_var`、`exclude_slash_tmp`）。
  - `ConfigToml::derive_sandbox_policy(sandbox_mode_override)` 将用户的 `sandbox_mode` 与补充字段转为最终 `SandboxPolicy`。
- `AskForApproval`（protocol）：
  - `OnRequest`（默认）、`OnFailure`、`UnlessTrusted`、`Never`；影响 `shell` 工具行为与审批流（`ExecApprovalRequestEvent`）。

### Shell 环境策略（env 构造）
- `ShellEnvironmentPolicyToml` → `ShellEnvironmentPolicy`：字段包括
  - `inherit`: `All`（默认）/`Core`/`None`
  - `ignore_default_excludes`：是否忽略默认过滤（包含“KEY”/“TOKEN”的变量）
  - `exclude`: 变量名匹配（wildmatch，不区分大小写）
  - `set`：显式设置键值对
  - `include_only`: 最终白名单
  - `experimental_use_profile`: 是否使用 shell profile（影响 `maybe_translate_shell_command` 包装）
- 构造顺序：继承 → 默认排除 → 自定义 `exclude` → 注入 `set` → `include_only` 过滤。

### 工作目录与信任
- `Config.cdw` 的解析：相对路径基于当前进程 cwd 解析为绝对路径。
- `ConfigToml::is_cwd_trusted(resolved_cwd)`：
  - 对比 `projects["/abs/path"].trust_level == "trusted"`。
  - 若在 Git worktree 下，根项目信任可被继承（`resolve_root_git_project_for_trust`）。
- 写入信任：`set_project_trusted(codex_home, project_path)` 自动改写 `config.toml` 的 `[projects."/path"]` 节以便 UI 侧/策略侧决策。

### 模型提供方（ModelProviderInfo）
- `model_providers`：
  - 内置：`openai`（Responses）、`oss`（本地/自建兼容 Chat 的开源服务，默认 `http://localhost:11434/v1`）。
  - 用户可在 `config.toml` 添加/覆盖：base_url、query_params、附加/环境 HTTP 头、`wire_api`、重试上限与 streaming idle 超时等。
- `create_request_builder(client, auth)`：
  - 解析 API key 或 OAuth（`codex-login`），支持 ChatGPT 模式；
  - 合成最终 URL（`/responses` vs `/chat/completions`）并注入头；
  - ChatGPT 模式时默认基址改为 `https://chatgpt.com/backend-api/codex`。
- 防御与上限：
  - `request_max_retries/stream_max_retries` 都有硬上限（100）。
  - `stream_idle_timeout_ms` 默认 300s，可调。

### 其他 Flags（flags.rs）
- `OPENAI_API_BASE`、`OPENAI_API_KEY`、`OPENAI_TIMEOUT_MS`（默认 300s）、`CODEX_RS_SSE_FIXTURE`（测试用离线 SSE 源）。

---

## 二、环境上下文注入（environment_context.rs）

### EnvironmentContext 内容与序列化
- 结构：`cwd`、`approval_policy`、`sandbox_mode`（protocol 枚举）、`network_access`（Enabled/Restricted）、`shell` 名。
- 构造：基于当前 `SandboxPolicy` 推导 `SandboxMode` 与 `NetworkAccess`。
- 序列化：输出为简单 XML 片段，包在 `<environment_context>...</environment_context>`，便于模型解析。
- 注入：转换为 `ResponseItem::Message { role: "user", content: [InputText] }`，由 Core 在 `Session::new` 初始回合记录；TUI/CLI 可以显示或隐藏。

---

## 三、协议层（codex-rs/protocol）

### 顶层模块
- `protocol.rs`：事件/提交类型主定义与工具相关事件。
- `models.rs`：模型输入/输出项（包括 FunctionCall、LocalShell、WebSearch 等）。
- `config_types.rs`：与客户端可见的 Reasoning/SandboxMode/ConfigProfile（子集）。
- `parse_command.rs`：显示层用的命令解析枚举（core 将自身解析结果转换为此枚举）。
- `plan_tool.rs`：update_plan 工具的参数类型。

### Submission 与 Op
- `Submission { id, op }` 与非穷尽的 `Op`：
  - `UserInput { items }` 与 `UserTurn { items, cwd, approval_policy, sandbox_policy, model, effort, summary }`
  - `OverrideTurnContext { cwd?, approval_policy?, sandbox_policy?, model?, effort?, summary? }` 用于持久上下文覆盖
  - 审批：`ExecApproval { id, decision }`、`PatchApproval { id, decision }`
  - 历史：`AddToHistory { text }`、`GetHistoryEntryRequest { offset, log_id }`、`GetHistory`
  - 列表：`ListMcpTools`、`ListCustomPrompts`；`Compact` 摘要；`Shutdown` 关闭

### 审批/沙箱策略（协议侧）
- `AskForApproval`：`UnlessTrusted`（历史兼容命名，实际表示“untrusted 模式”）、`OnFailure`、`OnRequest`（默认）、`Never`。
- `SandboxPolicy`：三种模式，提供工具函数：
  - `new_read_only_policy()`、`new_workspace_write_policy()`
  - `has_full_disk_read_access/has_full_disk_write_access/has_full_network_access()`
  - `get_writable_roots_with_cwd(cwd)`：为 WorkspaceWrite 计算可写根及其内部仍保持只读的子路径（如 `.git`）。

### 执行与补丁事件
- `ExecCommandBegin/End`：携带 `command/cwd/parsed_cmd` 与 `stdout/stderr/aggregated_output/exit_code/duration/formatted_output`。
- 增量输出：`ExecCommandOutputDeltaEvent { call_id, stream, chunk (bytes) }`（Core 有上限控制）。
- 审批请求：`ExecApprovalRequestEvent { call_id, command, cwd, reason? }`。
- 补丁审批与结果：
  - `ApplyPatchApprovalRequestEvent { call_id, changes, reason?, grant_root? }`
  - `PatchApplyBegin/End { changes, stdout/stderr, success }`
- 回合改动：`TurnDiffEvent { unified_diff }`。

### MCP 与会话事件
- MCP：`McpToolCallBegin/End { invocation{ server/tool/arguments }, duration, result }`，`McpListToolsResponse { tools }`。
- 历史：`ConversationHistoryResponse { conversation_id, entries }`、`GetHistoryEntryResponse { entry? }`。
- 会话配置：`SessionConfiguredEvent { session_id, model, history_log_id, history_entry_count }`。
- 其他：`BackgroundEventEvent`、`StreamErrorEvent`、中止：`TurnAborted { reason }`。

### 模型输入输出（models.rs）
- `ResponseItem`：
  - 文本/图片消息：`Message { id?, role, content: [ContentItem] }`
  - 推理项：`Reasoning { summary, content? 或 encrypted_content }`
  - 函数调用：`FunctionCall { name, arguments(raw JSON string), call_id }`
  - 工具输出：`FunctionCallOutput { call_id, output: FunctionCallOutputPayload }`
  - LocalShell（Chat 兼容）与 WebSearch 事件桥接
  - `CustomToolCall{ name,input }/CustomToolCallOutput`
  - 以及 `Other` 占位
- `ResponseInputItem`：供 Core 回写到模型输入（下一轮）的项。`From<Vec<InputItem>>` 会将本地图片（`LocalImage { path }`）读取为 data URL（base64 + MIME 推断），写入 `InputImage { image_url }`。
- `FunctionCallOutputPayload`：序列化为“纯字符串”（即使失败 `success=false`，也不包裹对象），以匹配上游 TS CLI 对 Responses API 的要求。
- `ShellToolCallParams`：`command[]`、`workdir`、`timeout_ms`、`with_escalated_permissions?`、`justification?`（Core 转为 `ExecParams` 并携带到 `process_exec_tool_call`）。

### 解析与计划
- `parse_command.rs::ParsedCommand`：Read/ListFiles/Search/Format/Test/Lint/Noop/Unknown（Core 的 `parse_command` 会填充 protocol 这一枚举供 UI 显示）。
- `plan_tool.rs::UpdatePlanArgs`：`explanation?` 与 `plan: [{ step, status }]`，被 TUI 消费。

---

## 四、端到端要点与常见问题

- 为什么需要同时有 core 侧与 protocol 侧类型？
  - Core 负责“实现与编排”，Protocol 负责“进程边界的契约”。Core 在内部使用自身更丰富的类型与工具，然后在向外发事件/收提交时映射到 protocol 的稳定结构。
- Responses vs Chat 差异：
  - `ModelProviderInfo.wire_api` 决定使用哪条路径；Responses 有工具/推理/流式字段差异（Core 在 `client.rs` 里桥接与聚合）。
- Shell 环境与 PowerShell/Profile：
  - 当 `ShellEnvironmentPolicy.use_profile` 或用户 shell 是 PowerShell 时，Core 会用 `maybe_translate_shell_command` 把默认 shell 调用包装成用户期望的形式，避免 PATH/profile 差异导致的行为偏差。
- 审批与沙箱：
  - `AskForApproval` 与 `SandboxPolicy` 组合影响是否弹出审批与是否在 Seatbelt/Landlock 下运行；出错时 Core 会做保守的“疑似沙箱拒绝”映射，便于 UI 展示与重试策略。

---

## 五、进一步阅读建议

- 回看 `core/src/codex.rs` 中 `Session::new` 如何注入 `EnvironmentContext` 与 `ConfigureSession`，以及 `handle_function_call` 对 `shell/exec_command/apply_patch/MCP/update_plan` 的分发。
- 对应 UI 消费路径可在 `codex-rs/tui` 中查看事件处理与渲染（快照测试提供很好的可视化对照）。

