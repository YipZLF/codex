# 模型与工具挂载深读（client_common.rs → client.rs → model_* → openai_tools.rs）

> 本文细读 core 中“模型请求构造、流式桥接、工具清单与 JSON-Schema 规范化”的关键实现，帮助你从 Prompt → 请求 → 流 → 工具 调用的视角准确把握数据与能力如何暴露给模型。

---

## Prompt 与基础指令（client_common.rs）

- 结构与职责
  - `Prompt { input, store, tools, base_instructions_override }`
  - 通过 `get_full_instructions(model_family)` 拼接最终“基础指令”文本：
    - 若无 `base_instructions_override`，且满足“模型需要特殊 apply_patch 指令”或“工具清单里未挂载 apply_patch”二者之一，会自动追加 `APPLY_PATCH_TOOL_INSTRUCTIONS`。
  - `format_user_instructions_message(ui)` 将自定义用户指令包裹成易于模型解析的段落（自定义标签）。
- Reasoning 与 Verbosity
  - `create_reasoning_param_for_request(model_family, effort, summary)`：仅对支持 reasoning summaries 的模型族返回值；否则为 `None`。
  - `create_text_param_for_request(verbosity)`：仅在 GPT-5 family 模型下由 `client.rs` 实际注入（见后）。
- Responses API 请求载体：`ResponsesApiRequest<'a>`
  - 字段：`model`、`instructions`、`input`、`tools`（已序列化 JSON）、`tool_choice="auto"`、`parallel_tool_calls=false`、`reasoning?`、`store`、`stream=true`、`include`、`prompt_cache_key=session_id`、`text?`。
- 流式桥接：`ResponseStream` 是一个自定义 `Stream`，底层用 `mpsc::channel` 转发解析后的增量事件。

---

## 模型客户端与流（client.rs）

- `ModelClient { config, auth_manager, client(reqwest), provider(ModelProviderInfo), session_id, effort, summary }`
- 双协议分发：`stream(prompt)`
  - `WireApi::Responses` → `stream_responses`：
    - `store`：当登录模式为 ChatGPT（`AuthMode::ChatGPT`）时强制 `store=false`；否则尊重 `prompt.store`。
    - `include`：当 `!store` 且启用 reasoning 时，追加 `"reasoning.encrypted_content"`，否则为空。
    - 仅在 family==gpt-5 时设置 `text.verbosity`；否则忽略并告警（若配置了 verbosity）。
    - 请求头：`OpenAI-Beta: responses=experimental`、`session_id`、`originator`（`Config.responses_originator_header`）、`User-Agent`（`get_codex_user_agent`）；ChatGPT 模式另加 `chatgpt-account-id`。
    - 稳健性：按 provider 的 `request_max_retries()` 与流 `stream_max_retries()`/`stream_idle_timeout()` 管理连接；配合 `util::backoff` 退避。
    - SSE：基于 `eventsource_stream`，将 Responses 的增量解析为 `ResponseEvent` 并通过 `mpsc` 转为 `ResponseStream`。
  - `WireApi::Chat`（Chat Completions 兼容层） →
    - 先拿原始事件流 `chat_completions::stream_chat_completions(..)`；
    - 再用 `AggregateStreamExt` 聚合为“每回合一个最终 assistant 消息”（或在 `show_raw_agent_reasoning` 下保持 streaming 模式）；
    - 同样通过 `mpsc` 桥接为 `ResponseStream`。

---

## 模型族与提供方（model_family.rs, openai_model_info.rs, model_provider_info.rs）

- ModelFamily（要点）
  - 字段：`slug`、`family`、`needs_special_apply_patch_instructions`、`supports_reasoning_summaries`、`uses_local_shell_tool`、`apply_patch_tool_type`。
  - 这些标志影响：是否自动追加 apply_patch 说明、是否使用 LocalShell 工具、apply_patch 工具形态（freeform/function）。
- ModelProviderInfo
  - `wire_api` 决定走 Responses 还是 Chat；
  - `create_request_builder(client, auth)` 合成请求（API key/OAuth/ChatGPT）、叠加静态/环境 HTTP 头、拼接 query 参数、选择最终 URL：
    - Responses → `.../v1/responses`
    - Chat → `.../v1/chat/completions`
    - ChatGPT 模式：默认基址替换为 `https://chatgpt.com/backend-api/codex`；
  - `request_max_retries`/`stream_max_retries`/`stream_idle_timeout_ms` 提供稳健性上限。

---

## 工具清单构建（openai_tools.rs）

- 配置输入：`ToolsConfigParams` → `ToolsConfig::new(..)`
  - 决定 Shell 工具类型：
    - `StreamableShell`（当 `use_streamable_shell_tool=true`）→ 挂载 `exec_command` 与 `write_stdin` 两个工具；
    - 否则若 `model_family.uses_local_shell_tool` → `LocalShell`；
    - 否则 `DefaultShell`；
    - 且在 `AskForApproval::OnRequest` 且非 streamable 时，切换为 `ShellWithRequest { sandbox_policy }`，在 schema/描述中明确“提权与理由”。
  - apply_patch 工具：
    - 若 `model_family.apply_patch_tool_type` 指定 → 采用该类型；
    - 否则当 `include_apply_patch_tool=true` → 默认 `Freeform`；否则不挂载。
  - 其他：可选挂载 `plan`、`web_search`、`view_image`（文件路径注入）。

- Shell 工具 schema/描述
  - `create_shell_tool()`：`command: string[]`、`workdir?: string`、`timeout_ms?: number`。
  - `create_shell_tool_for_sandbox(policy)`：在 `WorkspaceWrite`/`ReadOnly` 模式下追加：
    - `with_escalated_permissions?: boolean`、`justification?: string`；
    - 描述中列举需要提权的类别（跨目录读、写、.git/.env 写、构建/测试/包管理等），并根据 `network_access` 提示网络限制。

- Streamable Shell 工具
  - 从 `crate::exec_command` 暴露：
    - `create_exec_command_tool_for_responses_api()` 与 `create_write_stdin_tool_for_responses_api()`；
    - 前者启动 PTY 会话并返回 `Ongoing(session_id)`/`Exited(code)` 的人类可读输出；后者向会话写入并收集“从现在开始”的输出窗口。

- `view_image` 工具
  - 参数：`path: string`（本地文件系统路径）；Core 侧将其读入并嵌入 data URL（base64 + MIME）。

- MCP 工具整合
  - `get_openai_tools(config, mcp_tools)` 接收“全名 → Tool”的映射，输出排序后的工具清单（先内置，再按名字排序追加 MCP）。
  - `mcp_tool_to_openai_tool(name, tool)` 将 MCP schema 转为 OpenAI 工具。
  - `sanitize_json_schema(&mut value)` 规范化 JSON-Schema：
    - 确保每个对象都有 `type`；缺失时按常见特征推断（`properties/required/additionalProperties`→object，`items`→array，`enum/const/format`→string，数值范围→number）。
    - 若 `type` 为数组（union），择优挑首个受支持类型。
    - 为 `object` 确保 `properties` 存在；`additionalProperties` 若是对象也递归规范化。
    - 为 `array` 确保 `items` 存在（默认为 `{ type: "string" }`）。

- 工具 JSON 生成
  - `create_tools_json_for_responses_api(tools)` 将 `OpenAiTool` 列表序列化为 Responses API 期望的形态，供 `client.rs` 请求使用。

---

## 项目文档注入（project_doc.rs，关联 Prompt）

- 搜索：从会话 `cwd` 向上查找直至 Git 根（遇到 `.git` 停止），收集路径链上的所有 `AGENTS.md`。
- 截断：按 `Config.project_doc_max_bytes` 限制总字节数（超限警告并截断）。
- 合并：与 `Config.user_instructions` 通过分隔线 `--- project-doc ---` 拼接。
- 使用：`Codex::spawn` 中 `get_user_instructions(config)` 的返回作为会话起始提示的一部分。

---

## 端到端一览（典型回合）

1) Core 构造 `Prompt`（历史 + 用户/环境上下文 + 工具清单）。
2) `ModelClient::stream(..)` 发送请求（Responses/Chat），开启 SSE 或聚合流，转为 `ResponseStream`。
3) 模型若发起工具调用：
   - `shell/local_shell/streamable shell`：`codex.rs` 解析参数 → 安全/审批 → 执行（一次性或会话）→ 输出回填为 `FunctionCallOutputPayload`。
   - `apply_patch`：走审批与委托路径。
   - MCP：通过 `McpToolCallBegin/End` 包装请求与结果。
4) 将工具输出与后续 assistant 消息统一作为下一回合的输入项，驱动新的 `Prompt`。

---

## 常见注意点

- GPT-5 语族下才使用 `text.verbosity`；其他模型配置了也会被忽略并告警。
- ChatGPT 登录模式下强制 `store=false`；此时 `include` 会请求加密 reasoning 内容以避免引用 ID。
- 启用 `use_experimental_streamable_shell_tool` 时，模型需遵循“先 exec_command 建会话，再 write_stdin 交互”的节奏。
- MCP 工具 schema 兼容广泛，但在 Responses API 端需满足较严格的 `type`/`properties/items` 要求；规范化函数已覆盖常见情况。

