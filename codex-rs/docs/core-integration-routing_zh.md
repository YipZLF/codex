# Codex Core 接入与扩展指南（模型、Tool 转发、MCP）

本文回答：
- 如何配置并对接自部署模型服务（接口与协议）
- Tool 调用能否转发到云端服务（资源隔离）
- 如何接入其他 MCP 服务

并给出源码位置，便于将 codex‑core 作为“请求路由器”。

---

## 结论速览

- 模型服务：通过 `~/.codex/config.toml` 的 `model_providers` 配置 OpenAI 兼容端点，支持 Responses API 与 Chat Completions。
  - 代码：`core/src/config.rs`、`core/src/model_provider_info.rs`、`core/src/client.rs`、`core/src/chat_completions.rs`。
- Tool 转发到云端：推荐用 MCP。将远端能力包装成 MCP 工具，Codex 自动发现并把它们暴露给模型，调用后由 `McpConnectionManager` 透传到云端。
  - 代码：`core/src/mcp_connection_manager.rs`、`core/src/mcp_tool_call.rs`、`core/src/openai_tools.rs`、`core/src/codex.rs`。
- 接入 MCP：在 `config.toml` 的 `mcp_servers` 中声明服务启动命令，Codex 启动时拉起并聚合工具。注入给模型的名称为 `server__tool`。

---

## 一、对接自部署模型服务

### 1) 配置与选择
- 配置文件：`~/.codex/config.toml`
- 关键字段：`model`、`model_provider`、`model_providers.*`、`profiles.*`
- 模型族推导：`core/src/model_family.rs::find_family_for_model()`。未知 slug 会降级为“直通”模式（可用 `model_supports_reasoning_summaries` 显式声明是否支持 reasoning summary）。
- 配置合并：`core/src/config.rs::Config::load_from_base_config_with_overrides()`（按 `config.toml` / CLI 覆盖 / 代码默认合并）。
- Provider 合并：内置 provider 由 `built_in_model_providers()` 提供；`config.toml` 的 `model_providers` 仅“扩展”，不会覆盖同名内置项（见 `or_insert` 语义）。

### 2) Provider 定义与请求
- 结构体：`core/src/model_provider_info.rs::ModelProviderInfo`
  - `base_url`: OpenAI 兼容 API 根（不含 `/responses` 或 `/chat/completions`）
  - `wire_api`: `"responses"` 或 `"chat"`
  - `env_key`: 从该环境变量读取 API Key（为空时走 OAuth/ChatGPT 或无鉴权）
  - `http_headers` / `env_http_headers`: 静态/环境变量注入的 HTTP 头
  - `request_max_retries` / `stream_max_retries` / `stream_idle_timeout_ms`: 重试与空闲超时
  - URL 拼装：`get_full_url()` 会按 `wire_api` 追加 `/responses` 或 `/chat/completions`
  - 认证与头：`create_request_builder()` 注入 Bearer/OAuth/自定义头

- Responses API：`core/src/client.rs::ModelClient::stream_responses()`
  - SSE：`Accept: text/event-stream`，解析 `response.*` 事件（`process_sse()`）
  - 头：`OpenAI-Beta: responses=experimental`、`originator`、`session_id`、`User-Agent`
  - 工具 JSON：`openai_tools.rs::create_tools_json_for_responses_api()`

- Chat Completions：`core/src/chat_completions.rs::stream_chat_completions()`
  - SSE：`process_chat_sse()`，把增量聚合为最终 assistant 消息（或保留原始推理事件）
  - 工具 JSON：`openai_tools.rs::create_tools_json_for_chat_completions_api()`（从 Responses 形态适配）

- 执行入口：`core/src/codex.rs::run_turn()` 构造 `Prompt` 并发起 `ModelClient::stream()`；工具清单来自 `get_openai_tools()`（见下文 MCP 注入）。

### 3) 自部署服务配置示例（`config.toml`）
- Responses 兼容（自托管/代理）：
```toml
[model_providers.my-responses]
name = "My Responses Server"
base_url = "https://your-llm.example.com/v1"
wire_api = "responses"
env_key = "MY_LLM_API_KEY"
request_max_retries = 3
stream_max_retries = 5
stream_idle_timeout_ms = 300000

[profiles.selfhosted]
model = "my-model-slug"
model_provider = "my-responses"
model_supports_reasoning_summaries = true  # 若服务器支持 reasoning summary
```
- Chat 兼容（如 vLLM/Ollama/自建代理）：
```toml
[model_providers.oss]
name = "OSS"
base_url = "http://localhost:11434/v1"
wire_api = "chat"

[profiles.oss-llama3]
model = "llama3"          # 原样发至 chat.completions 的 model 字段
model_provider = "oss"
```
- Azure 风格（Chat）：
```toml
[model_providers.azure]
name = "Azure"
base_url = "https://xxxxx.openai.azure.com/openai"
wire_api = "chat"
env_key = "AZURE_OPENAI_API_KEY"
query_params = { api-version = "2025-04-01-preview" }

[profiles.azure-gpt]
model = "gpt-4o"
model_provider = "azure"
```

—

## 二、将 Tool 调用转发到云端服务（资源隔离）

### 1) 现有本地执行（了解现状）
- 一次性 shell：`core/src/exec.rs`（结合 `seatbelt.rs/landlock.rs` 做沙箱）；工具定义：`openai_tools.rs`
- 流式会话（PTY + stdin）：`core/src/exec_command/*`
- apply_patch：`core/src/apply_patch.rs`、`core/src/tool_apply_patch.rs`

以上均在本机/沙箱内执行，不做远端转发。

### 2) 推荐：使用 MCP 作为“云端工具总线”
- 思路：把“需要云端执行/隔离”的能力实现为 MCP Server 工具。Codex 启动时拉起 MCP、聚合工具并注入给模型。模型发起工具调用后，Codex 通过 MCP 协议把调用透传到云端。
- 启动与发现：`core/src/mcp_connection_manager.rs::McpConnectionManager::new()` → `list_all_tools()`（10s 超时）→ `qualify_tools()` 以 `server__tool` 命名（最长 64 字符，超长前缀+SHA1）。
- 注入给模型：`core/src/codex.rs::run_turn()` 中 `get_openai_tools(&tools_config, Some(sess.mcp_connection_manager.list_all_tools()))`；转换为 OpenAI 工具 JSON：`openai_tools.rs::mcp_tool_to_openai_tool()`。
- 调用链：模型 → `ResponseItem::FunctionCall` → `codex.rs::handle_function_call()` → 命中 MCP 名称解析 `mcp_connection_manager.parse_tool_name()` → `mcp_tool_call::handle_mcp_tool_call()`（发送 `McpToolCallBegin/End` 事件）→ 远端执行。
- 协议：Model Context Protocol（JSON‑RPC over stdio 等），客户端库 `codex_mcp_client`；调用 `client.call_tool(tool, arguments, timeout)`。

### 3) MCP 服务器配置（把执行放在云端）
```toml
[mcp_servers.cloud_exec]
command = "/usr/bin/python3"
args = ["-m", "my_cloud_runner_mcp"]
env = { CLOUD_TOKEN = "..." }
```
- 工具示例：
  - `submit_job`（镜像/命令/资源规格）→ 返回 `job_id`
  - `get_artifact`（`job_id`/`path`）→ 返回下载链接或摘要
- 设计建议：幂等 + 结构化 JSON Schema（必填/枚举/嵌套对象）+ 限制输出体积（必要时返回引用）。

### 4) 不建议把“shell 工具”改造成远端代理
- 内置 shell 工具契约面向本地/沙箱执行；若强行劫持为“远端代理”，需要自建额外协议、审批与审计，复杂度高。将远端能力以 MCP 工具呈现更清晰、与 UI/事件一致。

—

## 三、接入其他 MCP 服务（步骤）

### 1) 在 `config.toml` 声明并拉起 MCP 服务器
```toml
[mcp_servers.mytools]
command = "/path/to/my_mcp_server"
args = ["--flag1", "value"]
env = { API_KEY = "..." }
```
- 服务名校验：`^[a-zA-Z0-9_-]+$`（`mcp_connection_manager.rs::is_valid_mcp_server_name()`）
- 启动失败会通过错误事件提示（见 `core/src/codex.rs` 初始化）

### 2) 工具发现与注入
- `list_tools` 超时 10s；全量工具聚合后，经 `openai_tools.rs` 转为 OpenAI 工具并与本地工具合并
- 注入名称：`server__tool`（稳定排序提升 prompt 缓存命中）

### 3) 调用与事件
- Codex 发送 `McpToolCallBegin/End` 事件（含耗时/结果）到前端；失败时把错误消息打包到工具输出，便于模型自适应重试

### 4) MCP 工具设计要点
- 明确 JSON Schema；错误以结构化文本返回
- 长任务拆分：`submit` + `status/result`，避免一次调用阻塞过久
- 可观测：日志/trace id；输出尺寸受控

—

## 四、运行期切换与路由策略

- 运行期覆写：`core/src/codex.rs::Op::OverrideTurnContext`、`Op::UserTurn`
  - 可临时切换 `model`、`sandbox_policy`、`cwd`、`reasoning effort/summary`，Codex 会据此重建 `ModelClient` 与工具清单
- Profiles：在 `profiles.*` 预设不同 provider/审批/沙箱策略，一键切换路由

—

## 五、关键文件对照表

- 模型与请求
  - `core/src/config.rs`（配置加载/合并/Profiles）
  - `core/src/model_provider_info.rs`（Provider 定义、URL/头/重试与超时）
  - `core/src/client.rs`（Responses API SSE 客户端）
  - `core/src/chat_completions.rs`（Chat Completions SSE 客户端）
  - `core/src/client_common.rs`（Prompt、Reasoning、工具 JSON、text 选项）
- 工具与路由
  - `core/src/openai_tools.rs`（工具清单 JSON、MCP→OpenAI 转换、Local/Streamable shell、apply_patch/web_search/view_image）
  - `core/src/codex.rs`（事件主循环、工具调用分发、MCP 调用桥接、turn diff/通知）
- MCP 集成
  - `core/src/mcp_connection_manager.rs`（拉起/聚合工具/调用、命名规范与限长）
  - `core/src/mcp_tool_call.rs`（MCP 调用事件与结果封装）

—

## 附：常见配置片段速查

- Chat 兼容自建（vLLM/Ollama）：
```toml
[model_providers.oss]
name = "OSS"
base_url = "http://localhost:11434/v1"
wire_api = "chat"

[profiles.oss-llama3]
model = "llama3"
model_provider = "oss"
```

- Responses 兼容代理：
```toml
[model_providers.my-proxy]
name = "My Proxy"
base_url = "https://llm-proxy.example.com/v1"
wire_api = "responses"
env_key = "LLM_PROXY_API_KEY"
request_max_retries = 3
stream_max_retries = 5

[profiles.my-proxy]
model = "my-model"
model_provider = "my-proxy"
model_supports_reasoning_summaries = true
```

- MCP 云端执行器：
```toml
[mcp_servers.cloud]
command = "/usr/bin/python3"
args = ["-m", "my_cloud_mcp"]
env = { CLOUD_TOKEN = "xxxxx" }
```

> 如需，我可以按你的云平台/代理栈，提供一份更贴合的 `config.toml` 模板，并给出最小的 MCP “作业提交 + 结果获取”工具定义示例。

