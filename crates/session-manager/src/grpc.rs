use anyhow::{anyhow, Result};
use std::time::Duration;
use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing;

use crate::stream_json::OutputEvent;

pub mod proto {
    tonic::include_proto!("claude_agent");
}

use proto::agent_worker_client::AgentWorkerClient;
use proto::{
    agent_event, ExecuteRequest, HealthRequest, InterruptRequest, SendMessageRequest,
};

/// gRPC client that communicates with the Python Agent SDK worker.
/// Maps `AgentEvent` stream to the existing `OutputEvent` enum.
pub struct GrpcExecutor {
    client: AgentWorkerClient<Channel>,
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

    /// Execute a new prompt (first message in a session).
    /// Streams events to `output_tx` and returns the captured session_id.
    pub async fn execute(
        &mut self,
        prompt: &str,
        system_prompt_append: &str,
        permission_mode: &str,
        env: std::collections::HashMap<String, String>,
        output_tx: &mpsc::Sender<OutputEvent>,
    ) -> Result<Option<String>> {
        let request = ExecuteRequest {
            prompt: prompt.to_string(),
            permission_mode: permission_mode.to_string(),
            env,
            system_prompt_append: system_prompt_append.to_string(),
            max_turns: 0,
        };

        let response = self
            .client
            .execute(request)
            .await
            .map_err(|e| anyhow!("Execute RPC failed: {}", e))?;

        self.process_event_stream(response.into_inner(), output_tx)
            .await
    }

    /// Send a follow-up message to an existing session.
    /// Streams events to `output_tx`.
    pub async fn send_message(
        &mut self,
        session_id: &str,
        prompt: &str,
        output_tx: &mpsc::Sender<OutputEvent>,
    ) -> Result<Option<String>> {
        let request = SendMessageRequest {
            session_id: session_id.to_string(),
            prompt: prompt.to_string(),
        };

        let response = self
            .client
            .send_message(request)
            .await
            .map_err(|e| anyhow!("SendMessage RPC failed: {}", e))?;

        self.process_event_stream(response.into_inner(), output_tx)
            .await
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

    /// Process a stream of AgentEvents, mapping each to OutputEvent and sending to the channel.
    /// Returns the captured session_id (from SessionInit).
    async fn process_event_stream(
        &self,
        mut stream: tonic::Streaming<proto::AgentEvent>,
        output_tx: &mpsc::Sender<OutputEvent>,
    ) -> Result<Option<String>> {
        let mut session_id: Option<String> = None;

        while let Some(event) = stream
            .message()
            .await
            .map_err(|e| anyhow!("gRPC stream error: {}", e))?
        {
            let Some(event_variant) = event.event else {
                continue;
            };

            match event_variant {
                agent_event::Event::SessionInit(init) => {
                    session_id = Some(init.session_id.clone());
                    tracing::info!(session_id = %init.session_id, "gRPC: SessionInit received");
                }
                agent_event::Event::Text(text) => {
                    if text.is_partial {
                        // Skip partial text events — we use complete messages
                        continue;
                    }
                    // Split text into lines for OutputEvent::TextLine
                    for line in text.text.lines() {
                        if output_tx
                            .send(OutputEvent::TextLine(line.to_string()))
                            .await
                            .is_err()
                        {
                            return Ok(session_id);
                        }
                    }
                    // Handle text that doesn't end with newline
                    if !text.text.ends_with('\n') && !text.text.is_empty() {
                        // The last "line" was already emitted by .lines()
                    }
                }
                agent_event::Event::ToolUse(tool) => {
                    // Format tool action for display
                    let action = format_grpc_tool_action(&tool.tool_name, &tool.input_json);
                    if output_tx
                        .send(OutputEvent::ToolAction(action))
                        .await
                        .is_err()
                    {
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
                    let input_tokens = result.input_tokens;
                    let output_tokens = result.output_tokens;

                    // Emit ProcessingStarted with input tokens
                    let _ = output_tx
                        .send(OutputEvent::ProcessingStarted { input_tokens })
                        .await;

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
                }
                agent_event::Event::Error(err) => {
                    tracing::error!(
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
                }
            }
        }

        Ok(session_id)
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
