use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use codex_core::ConversationManager;
use codex_core::config::Config;
use mcp_types::JSONRPCMessage;
use tokio::sync::mpsc;

use crate::message_processor::MessageProcessor;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::OutgoingMessageSender;

async fn serve_connection(
    stream: TcpStream,
    conversation_manager: Arc<ConversationManager>,
    codex_linux_sandbox_exe: Option<PathBuf>,
    config: Arc<Config>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Channels for outgoing messages
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<OutgoingMessage>();
    let outgoing_sender = OutgoingMessageSender::new(outgoing_tx.clone());
    let error_sender = OutgoingMessageSender::new(outgoing_tx.clone());
    // Build a fresh message processor for this connection
    let mut processor = MessageProcessor::with_conversation_manager(
        outgoing_sender,
        codex_linux_sandbox_exe,
        config,
        conversation_manager,
    );
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<JSONRPCMessage>(128);

    let processor_task = tokio::spawn(async move {
        while let Some(msg) = incoming_rx.recv().await {
            match msg {
                JSONRPCMessage::Request(rq) => {
                    processor.process_request(rq).await;
                }
                JSONRPCMessage::Response(resp) => {
                    processor.process_response(resp).await;
                }
                JSONRPCMessage::Notification(n) => {
                    processor.process_notification(n).await;
                }
                JSONRPCMessage::Error(err) => {
                    processor.process_error(err);
                }
            }
        }
    });

    // Writer task: forward OutgoingMessage to the socket as line‑delimited JSON
    let writer_task = {
        tokio::spawn(async move {
            while let Some(msg) = outgoing_rx.recv().await {
                let json_msg: JSONRPCMessage = msg.into();
                if let Ok(line) = serde_json::to_string(&json_msg) {
                    if writer.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    if writer.write_all(b"\n").await.is_err() {
                        break;
                    }
                    if writer.flush().await.is_err() {
                        break;
                    }
                }
            }
        })
    };

    // Reader loop: parse each line as JSONRPCMessage and forward to processor
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<JSONRPCMessage>(&line) {
                    Ok(msg) => {
                        if incoming_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        // Respond with JSON-RPC parse error (-32700). Use a synthetic id since none was parsed.
                        let _ = error_sender
                            .send_error(
                                mcp_types::RequestId::String("parse".to_string()),
                                mcp_types::JSONRPCErrorError {
                                    code: -32700,
                                    message: format!("parse error: {e}"),
                                    data: None,
                                },
                            )
                            .await;
                    }
                }
            }
            Err(_) => break,
        }
    }

    let _ = processor_task.abort();
    let _ = writer_task.abort();
}

/// Start a TCP MCP server bound to `bind_addr` that reuses the provided
/// ConversationManager so TUI and MCP share the same sessions.
pub async fn serve_tcp(
    bind_addr: &str,
    conversation_manager: Arc<ConversationManager>,
    codex_linux_sandbox_exe: Option<PathBuf>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind_addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let cm = conversation_manager.clone();
        let cfg = config.clone();
        let exe = codex_linux_sandbox_exe.clone();
        tokio::spawn(async move {
            serve_connection(stream, cm, exe, cfg).await;
        });
    }
}
