use std::time::Duration;

use mcp_test_support::McpProcess;
use mcp_test_support::create_final_assistant_message_sse_response;
use mcp_test_support::create_mock_chat_completions_server;
use mcp_types::JSONRPC_VERSION;
use mcp_types::JSONRPCNotification;
use mcp_types::JSONRPCResponse;
use mcp_types::RequestId;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_m1_tools_status_and_inject() {
    // Mock two turns: initial and injected turn
    let server = create_mock_chat_completions_server(vec![
        create_final_assistant_message_sse_response("First").unwrap(),
        create_final_assistant_message_sse_response("Second").unwrap(),
    ])
    .await;

    // Build config.toml pointing to mock provider
    let codex_home = TempDir::new().unwrap();
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_policy = "read-only"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{}/v1"
wire_api = "chat"
request_max_retries = 0
stream_max_retries = 0
"#,
            server.uri()
        ),
    )
    .unwrap();

    let mut mcp = McpProcess::new(codex_home.path()).await.unwrap();
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize())
        .await
        .unwrap()
        .unwrap();

    // 1) Start a new session via `codex` and capture session_id from SessionConfigured notification
    let codex_request_id = mcp
        .send_call_tool_request(
            "codex",
            json!({
                "prompt": "start",
            }),
        )
        .await
        .unwrap();

    // Read until we see a SessionConfigured notification to obtain session_id
    let session_id = read_session_configured_session_id(&mut mcp)
        .await
        .expect("session configured notification with session_id");

    // Wait for codex to finish first turn
    let _resp = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(codex_request_id)),
    )
    .await
    .unwrap()
    .unwrap();

    // 2) Query status
    let status_id = mcp
        .send_call_tool_request("codex-status", json!({ "sessionId": session_id }))
        .await
        .unwrap();
    let status_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(status_id)),
    )
    .await
    .unwrap()
    .unwrap();
    // Expect exists: true
    assert_eq!(
        JSONRPCResponse {
            jsonrpc: JSONRPC_VERSION.into(),
            id: RequestId::Integer(status_id),
            result: json!({"exists": true}),
        },
        status_resp
    );

    // 3) Inject a new prompt; expect it completes with "Second"
    let inject_id = mcp
        .send_call_tool_request(
            "codex-inject",
            json!({
                "sessionId": session_id,
                "prompt": "again",
            }),
        )
        .await
        .unwrap();
    let inject_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(inject_id)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(inject_resp.id, RequestId::Integer(inject_id));
    let result = inject_resp.result;
    assert_eq!(
        result.get("content"),
        Some(&json!([{"type":"text","text":"Second"}]))
    );
    if let Some(sc) = result.get("structured_content") {
        assert!(sc.get("session_id").is_some());
    }
}

async fn read_session_configured_session_id(mcp: &mut McpProcess) -> Option<String> {
    loop {
        let notification: JSONRPCNotification = mcp
            .read_stream_until_notification_message("codex/event")
            .await
            .ok()?;
        if let Some(p) = notification.params {
            let event_type = p
                .get("msg")
                .and_then(|m| m.get("type"))
                .and_then(|t| t.as_str());
            if event_type == Some("session_configured") {
                if let Some(sid) = p
                    .get("msg")
                    .and_then(|m| m.get("session_id"))
                    .and_then(|s| s.as_str())
                {
                    return Some(sid.to_string());
                }
            }
        }
    }
}
