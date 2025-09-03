# 在 TUI 与程序中使用 MCP Client 的实战指南

本文解答两个问题：
1) “如何在 TUI 里面启动 mcp-client？”
2) “mcp-client 用户应该怎么用（程序化调用）？”

核心结论：
- 在 TUI/交互式场景下，你无需手工“启动 mcp-client”。只要在配置里声明要连接/拉起哪些 MCP 服务器，codex-core 会在会话初始化时为每个服务器创建一个 `McpClient` 并完成初始化；这些服务器提供的工具将自动出现在模型可调用的工具列表中。
- 在程序化编排场景下，直接使用 `codex-mcp-client`（库）来拉起 MCP 服务器（含 Codex 自身），然后按 MCP 规范发 `initialize`、`tools/list`、`tools/call` 等请求即可。

---

## 一、在 TUI 中“启动 mcp-client”的正确姿势

TUI 本身并不显示/暴露“mcp-client”开关。正确做法是在配置中声明 MCP 服务器列表，codex-core 会在 `Session::new` 时通过 `McpConnectionManager` 自动为每个服务器启动一个 `McpClient` 并完成 `initialize`：

- 配置文件：`~/.codex/config.toml`
- 配置段：`[mcp_servers]`（键为“服务器名”，值为命令行与环境）

示例：在 TUI 中把“Codex 自身”作为一个 MCP 服务器接入（从而允许主 Agent 通过 MCP 工具再启动/控制子 Agent）。

```
[mcp_servers.codex]
command = "codex"
args = ["mcp"]
# 可选：注入环境变量，或替换为你自定义的 MCP 服务器
# env = { RUST_LOG = "info" }
```

效果：
- TUI 启动后，core 会调用 `McpConnectionManager::new()`，为上例的 `codex` 启动一个 MCP 子进程并连接；
- 该服务器提供的工具（例如 `codex`、`codex-reply`）会被自动汇聚到模型可用的工具列表，其名称会被“服务器名 + 分隔符 + 工具名”做限定，形如：
  - `codex__codex`
  - `codex__codex-reply`
- 模型在回合中可直接调用这些工具，从而实现“由主 Agent 驱动子 Agent”。

注意：
- 若 `mcp_servers` 配置错误，初始化阶段会在事件流中收到“某个 MCP 客户端启动失败”的错误提示；修正后重启即可。
- 工具名会被限定在一定长度内并可能带 hash 后缀（参见核心的工具名限定逻辑），这是为了满足上游 API 的命名约束。

---

## 二、作为用户/编排器直接使用 codex-mcp-client（库）

`codex-mcp-client` 是一个轻量异步库，不提供单独的 CLI。典型使用流程如下：

- 依赖：在同一工作区内直接引用 `codex-mcp-client` crate（本仓库已包含）。
- 启动与初始化：
  1) `McpClient::new_stdio_client(program, args, env)` 拉起一个 MCP 服务器子进程（如 `codex mcp`），并建立 stdio 通道；
  2) `initialize(params, notification_params, timeout)` 发送初始化请求并完成握手；
  3) `list_tools(None, Some(timeout))` 获取工具清单；
  4) `call_tool("codex", arguments, Some(timeout))` 发起工具调用；

示例（最小可运行骨架）：

```
use std::time::Duration;
use codex_mcp_client::McpClient;
use mcp_types::{InitializeRequestParams, ClientCapabilities, Implementation, MCP_SCHEMA_VERSION};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1) 拉起 Codex 的 MCP 服务器（等价命令：`codex mcp`）
    let client = McpClient::new_stdio_client(
        std::ffi::OsString::from("codex"),
        vec![std::ffi::OsString::from("mcp")],
        None, // 可选: 注入特定环境变量
    )
    .await?;

    // 2) initialize（客户端能力里带上 elicitation 空对象，符合规范）
    let init_params = InitializeRequestParams {
        capabilities: ClientCapabilities {
            experimental: None,
            roots: None,
            sampling: None,
            elicitation: Some(json!({})),
        },
        client_info: Implementation {
            name: "my-orchestrator".into(),
            version: "0.1.0".into(),
            title: Some("My Orchestrator".into()),
        },
        protocol_version: MCP_SCHEMA_VERSION.to_string(),
    };
    client
        .initialize(init_params, None, Some(Duration::from_secs(10)))
        .await?;

    // 3) tools/list
    let tools = client
        .list_tools(None, Some(Duration::from_secs(10)))
        .await?;
    println!("tools: {:?}", tools.tools.iter().map(|t| &t.name).collect::<Vec<_>>());

    // 4) tools/call - 调用 Codex 工具新建会话并执行一次
    let args = json!({
        "prompt": "请总结 README.md 的要点",
        "approval-policy": "never",
        "sandbox": "read-only",
        // 可选: 针对会话的细粒度覆盖，等价 -c dotted paths
        "config": {
            "model": "gpt-4.1-mini"
        }
    });
    let result = client
        .call_tool("codex".to_string(), Some(args), Some(Duration::from_secs(120)))
        .await?;
    println!("result: {:?}", result);

    Ok(())
}
```

- 续写同一会话：使用 `codex-reply` 工具，传入 `session_id` 与新 `prompt` 即可；或者使用服务器提供的 Codex 扩展 RPC（参考 mcp-server 的 `codex_message_processor.rs`）。
- 超时与错误：所有请求均支持超时；错误将以 JSON‑RPC 错误形式返回，或者通过 `CallToolResult.is_error = true` 表示工具级错误。

实践建议：
- 批量/无人值守：将 `approval-policy` 设为 `never`，并限制 `sandbox` 为 `read-only` 或 `workspace-write`。
- 多 Agent：在同一服务器内创建多会话即可；如需强隔离，可在 orchestrator 中拉起多个 `codex mcp` 子进程（不同 cwd/策略）。

---

## 三、常见问题（FAQ）

- Q: 能否“在 TUI 里挂载/查看 MCP 服务器内部的会话”？
  - A: 现有 TUI 默认直接驱动本地 core；它不“附着”到 MCP 服务器里的会话。若需要观察 MCP 服务器内部的子 Agent，建议编写一个“作为 MCP 客户端的轻量 TUI/监控器”，通过 `AddConversationListener` 等扩展方法订阅事件流并渲染。

- Q: 工具名为什么是 `server__tool` 这种形式？
  - A: 为了在多服务器聚合工具时保持唯一性并满足上游 API 的命名约束，core 会进行“服务器名 + 分隔符 + 工具名”的限定与长度裁剪。

- Q: 如何在 TUI 里让主 Agent 调用子 Agent？
  - A: 按第一节配置好 `[mcp_servers]` 后，主 Agent 就能在推理中调用例如 `codex__codex` 工具，并通过其参数控制子 Agent 的模型、审批策略、沙箱与 cwd 等。

---

## 四、相关源码位置

- TUI 使用的 MCP 连接管理（自动拉起/聚合工具）
  - `core/src/mcp_connection_manager.rs`
- 会话初始化时触发 MCP 连接与错误汇报
  - `core/src/codex.rs::Session::new()`
- MCP 服务器（Codex 作为服务端）
  - 入口：`mcp-server/src/lib.rs::run_main`
  - 请求分发：`mcp-server/src/message_processor.rs`
  - 工具 schema/会话配置：`mcp-server/src/codex_tool_config.rs`
  - 工具执行/事件转发/审批桥接：`mcp-server/src/codex_tool_runner.rs`, `exec_approval.rs`, `patch_approval.rs`
- MCP 客户端（库）
  - `mcp-client/src/mcp_client.rs`

