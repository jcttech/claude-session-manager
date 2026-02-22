use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing;

use crate::stream_json::OutputEvent;

pub mod proto {
    tonic::include_proto!("claude_agent");
}

use proto::agent_worker_client::AgentWorkerClient;
use proto::{agent_event, HealthRequest, InterruptRequest};

/// gRPC client that communicates with the Python Agent SDK worker.
pub struct GrpcExecutor {
    client: AgentWorkerClient<Channel>,
}

/// A bidirectional gRPC session. One stream = one SDK session lifetime.
/// Send CreateSession or FollowUp requests, then process turn events.
pub struct GrpcSession {
    request_tx: mpsc::Sender<proto::SessionInput>,
    response_stream: tonic::Streaming<proto::AgentEvent>,
}

impl GrpcExecutor {
    /// Connect to a worker at the given address (e.g. "http://host:50051").
    pub async fn connect(addr: &str) -> Result<Self> {
        let channel = Channel::from_shared(addr.to_string())
            .map_err(|e| anyhow!("Invalid gRPC address '{}': {}", addr, e))?
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(600)) // 10 min max per RPC
            .connect()
            .await
            .map_err(|e| anyhow!("Failed to connect to worker at {}: {}", addr, e))?;

        Ok(Self {
            client: AgentWorkerClient::new(channel),
        })
    }

    /// Open a bidirectional Session stream. Returns handles for sending
    /// requests and processing events.
    pub async fn open_session(&mut self) -> Result<GrpcSession> {
        let (tx, rx) = mpsc::channel(16);
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        tracing::debug!("gRPC: Sending Session RPC (waiting for response headers)");
        let response = self
            .client
            .session(stream)
            .await
            .map_err(|e| anyhow!("Session RPC failed: {}", e))?;
        tracing::debug!("gRPC: Session RPC connected (response headers received)");
        Ok(GrpcSession {
            request_tx: tx,
            response_stream: response.into_inner(),
        })
    }

    /// Interrupt a running session.
    pub async fn interrupt(&mut self, session_id: &str) -> Result<bool> {
        let request = InterruptRequest {
            session_id: session_id.to_string(),
        };

        let response = self
            .client
            .interrupt(request)
            .await
            .map_err(|e| anyhow!("Interrupt RPC failed: {}", e))?;

        Ok(response.into_inner().success)
    }

    /// Health check — returns (ready, version).
    pub async fn health(&mut self) -> Result<(bool, String)> {
        let response = self
            .client
            .health(HealthRequest {})
            .await
            .map_err(|e| anyhow!("Health RPC failed: {}", e))?;

        let resp = response.into_inner();
        Ok((resp.ready, resp.worker_version))
    }
}

impl GrpcSession {
    /// Send a CreateSession request (first message).
    pub async fn create(
        &mut self,
        prompt: &str,
        permission_mode: &str,
        env: HashMap<String, String>,
        system_prompt_append: &str,
        max_thinking_tokens: i32,
    ) -> Result<()> {
        let input = proto::SessionInput {
            input: Some(proto::session_input::Input::Create(proto::CreateSession {
                prompt: prompt.to_string(),
                permission_mode: permission_mode.to_string(),
                env,
                system_prompt_append: system_prompt_append.to_string(),
                max_turns: 0,
                max_thinking_tokens,
            })),
        };
        self.request_tx
            .send(input)
            .await
            .map_err(|_| anyhow!("Session request channel closed"))
    }

    /// Send a FollowUp request (subsequent messages).
    pub async fn follow_up(&mut self, prompt: &str) -> Result<()> {
        let input = proto::SessionInput {
            input: Some(proto::session_input::Input::FollowUp(proto::FollowUp {
                prompt: prompt.to_string(),
            })),
        };
        self.request_tx
            .send(input)
            .await
            .map_err(|_| anyhow!("Session request channel closed"))
    }

    /// Read events from the response stream until SessionResult.
    /// Returns captured session_id. Stream stays open for next turn.
    pub async fn process_turn_events(
        &mut self,
        output_tx: &mpsc::Sender<OutputEvent>,
    ) -> Result<Option<String>> {
        let mut session_id: Option<String> = None;
        let mut event_count: u64 = 0;
        // Buffer for accumulating partial text deltas into complete lines
        let mut partial_buf = String::new();

        tracing::debug!("gRPC: Starting turn event processing");

        loop {
            match self.response_stream.message().await {
                Ok(Some(event)) => {
                    event_count += 1;
                    let Some(event_variant) = event.event else {
                        tracing::debug!(event_count, "gRPC: Empty event (no variant)");
                        continue;
                    };

                    match event_variant {
                        agent_event::Event::SessionInit(init) => {
                            session_id = Some(init.session_id.clone());
                            tracing::info!(session_id = %init.session_id, "gRPC: SessionInit received");
                        }
                        agent_event::Event::Text(text) => {
                            if text.is_partial {
                                // Accumulate partial text and emit complete lines in real-time
                                partial_buf.push_str(&text.text);
                                while let Some(newline_pos) = partial_buf.find('\n') {
                                    let line = partial_buf[..newline_pos].to_string();
                                    partial_buf = partial_buf[newline_pos + 1..].to_string();
                                    if output_tx
                                        .send(OutputEvent::TextLine(line))
                                        .await
                                        .is_err()
                                    {
                                        tracing::warn!(event_count, "gRPC: output_tx closed during partial text");
                                        return Ok(session_id);
                                    }
                                }
                            } else {
                                // Complete text — flush any remaining partial buffer first
                                if !partial_buf.is_empty()
                                    && output_tx
                                        .send(OutputEvent::TextLine(std::mem::take(&mut partial_buf)))
                                        .await
                                        .is_err()
                                {
                                    return Ok(session_id);
                                }
                                let line_count = text.text.lines().count();
                                tracing::info!(event_count, line_count, "gRPC: Complete text received");
                                for line in text.text.lines() {
                                    if output_tx
                                        .send(OutputEvent::TextLine(line.to_string()))
                                        .await
                                        .is_err()
                                    {
                                        tracing::warn!(event_count, "gRPC: output_tx closed during complete text");
                                        return Ok(session_id);
                                    }
                                }
                            }
                        }
                        agent_event::Event::ToolUse(tool) => {
                            // Flush any remaining partial text before tool action
                            if !partial_buf.is_empty() {
                                let _ = output_tx
                                    .send(OutputEvent::TextLine(std::mem::take(&mut partial_buf)))
                                    .await;
                            }
                            tracing::info!(event_count, tool = %tool.tool_name, "gRPC: ToolUse received");
                            let action = format_grpc_tool_action(&tool.tool_name, &tool.input_json);
                            if output_tx
                                .send(OutputEvent::ToolAction(action))
                                .await
                                .is_err()
                            {
                                tracing::warn!(event_count, "gRPC: output_tx closed during tool action");
                                return Ok(session_id);
                            }
                        }
                        agent_event::Event::ToolResult(_) => {
                            // Tool results are informational; not surfaced to chat
                        }
                        agent_event::Event::Subagent(sub) => {
                            let action = if sub.is_start { "started" } else { "finished" };
                            tracing::debug!(
                                agent = %sub.agent_name,
                                parent_tool = %sub.parent_tool_use_id,
                                action,
                                "Subagent event"
                            );
                        }
                        agent_event::Event::Result(result) => {
                            // Flush any remaining partial text
                            if !partial_buf.is_empty() {
                                let _ = output_tx
                                    .send(OutputEvent::TextLine(std::mem::take(&mut partial_buf)))
                                    .await;
                            }
                            let input_tokens = result.input_tokens;
                            let output_tokens = result.output_tokens;
                            tracing::info!(
                                event_count,
                                input_tokens,
                                output_tokens,
                                is_error = result.is_error,
                                result_text_len = result.result_text.len(),
                                "gRPC: Result received (turn complete)"
                            );

                            if result.is_error {
                                let _ = output_tx
                                    .send(OutputEvent::ProcessDied {
                                        exit_code: Some(1),
                                        signal: None,
                                    })
                                    .await;
                            } else {
                                let _ = output_tx
                                    .send(OutputEvent::ResponseComplete {
                                        input_tokens,
                                        output_tokens,
                                    })
                                    .await;
                            }
                            // Turn is complete — return but keep stream open
                            tracing::info!(event_count, "gRPC: Turn event processing complete");
                            return Ok(session_id);
                        }
                        agent_event::Event::Error(err) => {
                            tracing::error!(
                                event_count,
                                error_type = %err.error_type,
                                message = %err.message,
                                "gRPC: Agent error"
                            );
                            let _ = output_tx
                                .send(OutputEvent::ProcessDied {
                                    exit_code: Some(1),
                                    signal: Some(format!("{}: {}", err.error_type, err.message)),
                                })
                                .await;
                            // Error terminates the turn
                            return Ok(session_id);
                        }
                    }
                }
                Ok(None) => {
                    // Stream ended (server closed)
                    tracing::info!(event_count, "gRPC: Response stream ended (server closed)");
                    // Flush any remaining partial text
                    if !partial_buf.is_empty() {
                        let _ = output_tx
                            .send(OutputEvent::TextLine(std::mem::take(&mut partial_buf)))
                            .await;
                    }
                    return Err(anyhow!("gRPC stream closed by server before SessionResult"));
                }
                Err(e) => {
                    tracing::error!(event_count, error = %e, "gRPC: Stream error");
                    return Err(anyhow!("gRPC stream error: {}", e));
                }
            }
        }
    }
}

/// Format a tool action from gRPC ToolUse data.
/// Parses the input_json to extract key parameters for display.
fn format_grpc_tool_action(tool_name: &str, input_json: &str) -> String {
    let input: serde_json::Value = serde_json::from_str(input_json).unwrap_or_default();

    // Reuse the same formatting logic from stream_json
    crate::stream_json::format_tool_action(tool_name, &input)
}

/// Wait for the worker health check to succeed, with retries.
pub async fn wait_for_health(addr: &str, max_retries: u32, retry_interval: Duration) -> Result<()> {
    for attempt in 1..=max_retries {
        match GrpcExecutor::connect(addr).await {
            Ok(mut executor) => {
                match executor.health().await {
                    Ok((true, version)) => {
                        tracing::info!(
                            addr,
                            version = %version,
                            attempt,
                            "Worker health check passed"
                        );
                        return Ok(());
                    }
                    Ok((false, _)) => {
                        tracing::debug!(addr, attempt, "Worker not ready yet");
                    }
                    Err(e) => {
                        tracing::debug!(addr, attempt, error = %e, "Health check failed");
                    }
                }
            }
            Err(e) => {
                tracing::debug!(addr, attempt, error = %e, "Failed to connect for health check");
            }
        }

        if attempt < max_retries {
            tokio::time::sleep(retry_interval).await;
        }
    }

    Err(anyhow!(
        "Worker at {} failed health check after {} attempts",
        addr,
        max_retries
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_grpc_tool_action_read() {
        let result = format_grpc_tool_action("Read", r#"{"file_path": "/src/main.rs"}"#);
        assert_eq!(result, "**Read** `/src/main.rs`");
    }

    #[test]
    fn test_format_grpc_tool_action_bash() {
        let result = format_grpc_tool_action("Bash", r#"{"command": "cargo test"}"#);
        assert_eq!(result, "**Bash** `cargo test`");
    }

    #[test]
    fn test_format_grpc_tool_action_invalid_json() {
        let result = format_grpc_tool_action("Read", "not json");
        assert_eq!(result, "**Read** `?`");
    }

    #[test]
    fn test_format_grpc_tool_action_mcp() {
        let result = format_grpc_tool_action("mcp__server__tool_name", "{}");
        assert_eq!(result, "**MCP** _tool_name_");
    }
}
