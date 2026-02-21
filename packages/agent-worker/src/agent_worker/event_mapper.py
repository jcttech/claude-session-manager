"""Map Claude Agent SDK message types to protobuf AgentEvent messages."""

from __future__ import annotations

import json
import time
from typing import TYPE_CHECKING

from . import agent_pb2

if TYPE_CHECKING:
    from claude_agent_sdk.types import (
        AssistantMessage,
        ResultMessage,
        StreamEvent,
        SystemMessage,
    )


def map_system_message(msg: SystemMessage) -> agent_pb2.AgentEvent | None:
    """Map a SystemMessage to an AgentEvent.

    Only the 'init' subtype produces an event (SessionInit).
    """
    if msg.subtype == "init":
        session_id = msg.data.get("session_id", "")
        return agent_pb2.AgentEvent(
            session_init=agent_pb2.SessionInit(session_id=session_id)
        )
    return None


def map_assistant_message(msg: AssistantMessage) -> list[agent_pb2.AgentEvent]:
    """Map an AssistantMessage to a list of AgentEvents.

    Each content block (text, tool_use, tool_result) becomes a separate event.
    """
    from claude_agent_sdk.types import TextBlock, ToolResultBlock, ToolUseBlock

    events = []
    for block in msg.content:
        if isinstance(block, TextBlock):
            events.append(
                agent_pb2.AgentEvent(
                    text=agent_pb2.TextContent(text=block.text, is_partial=False)
                )
            )
        elif isinstance(block, ToolUseBlock):
            events.append(
                agent_pb2.AgentEvent(
                    tool_use=agent_pb2.ToolUse(
                        tool_name=block.name,
                        tool_use_id=block.id,
                        input_json=json.dumps(block.input),
                    )
                )
            )
        elif isinstance(block, ToolResultBlock):
            events.append(
                agent_pb2.AgentEvent(
                    tool_result=agent_pb2.ToolResult(
                        tool_use_id=block.tool_use_id,
                        is_error=block.is_error or False,
                    )
                )
            )
    return events


def map_result_message(
    msg: ResultMessage, start_time: float
) -> agent_pb2.AgentEvent:
    """Map a ResultMessage to a SessionResult AgentEvent."""
    usage = msg.usage or {}
    duration_ms = int((time.monotonic() - start_time) * 1000)

    return agent_pb2.AgentEvent(
        result=agent_pb2.SessionResult(
            session_id=msg.session_id or "",
            input_tokens=usage.get("input_tokens", 0),
            output_tokens=usage.get("output_tokens", 0),
            cost_usd=msg.total_cost_usd or 0.0,
            num_turns=msg.num_turns or 0,
            duration_ms=duration_ms,
            is_error=msg.is_error or False,
            result_text=msg.result or "",
        )
    )


def map_stream_event(event: StreamEvent) -> agent_pb2.AgentEvent | None:
    """Map a StreamEvent to an AgentEvent.

    StreamEvents contain partial messages and subagent tracking.
    Handles the full SDK event flow:
    - content_block_start (tool_use) → ToolUse event for live card
    - content_block_delta (text_delta) → partial TextContent
    - content_block_start/message_stop with parent_tool_use_id → SubagentEvent
    """
    raw = event.event or {}
    event_type = raw.get("type", "")

    # --- Subagent tracking (has parent_tool_use_id) ---
    if event.parent_tool_use_id is not None:
        if event_type == "content_block_start":
            return agent_pb2.AgentEvent(
                subagent=agent_pb2.SubagentEvent(
                    agent_name=raw.get("content_block", {}).get("name", "subagent"),
                    parent_tool_use_id=event.parent_tool_use_id,
                    is_start=True,
                )
            )
        elif event_type == "message_stop":
            return agent_pb2.AgentEvent(
                subagent=agent_pb2.SubagentEvent(
                    agent_name="",
                    parent_tool_use_id=event.parent_tool_use_id,
                    is_start=False,
                )
            )

    # --- Tool use starting (content_block_start with tool_use type) ---
    if event_type == "content_block_start":
        content_block = raw.get("content_block", {})
        if content_block.get("type") == "tool_use":
            return agent_pb2.AgentEvent(
                tool_use=agent_pb2.ToolUse(
                    tool_name=content_block.get("name", ""),
                    tool_use_id=content_block.get("id", ""),
                    input_json="{}",  # Full input arrives via AssistantMessage later
                )
            )

    # --- Text delta streaming ---
    if event_type == "content_block_delta":
        delta = raw.get("delta", {})
        if delta.get("type") == "text_delta":
            return agent_pb2.AgentEvent(
                text=agent_pb2.TextContent(
                    text=delta.get("text", ""),
                    is_partial=True,
                )
            )

    # message_start, message_stop, message_delta, content_block_stop,
    # input_json_delta: not needed for live card updates
    return None


def error_event(message: str, error_type: str = "internal") -> agent_pb2.AgentEvent:
    """Create an AgentError event."""
    return agent_pb2.AgentEvent(
        error=agent_pb2.AgentError(message=message, error_type=error_type)
    )


def fallback_result_event(
    raw_data: dict, start_time: float
) -> agent_pb2.AgentEvent:
    """Build a SessionResult from raw dict when ResultMessage fails to parse.

    Uses .get() with safe defaults for all fields so partial data doesn't crash.
    """
    usage = raw_data.get("usage", {}) or {}
    duration_ms = int((time.monotonic() - start_time) * 1000)

    return agent_pb2.AgentEvent(
        result=agent_pb2.SessionResult(
            session_id=raw_data.get("session_id", ""),
            input_tokens=usage.get("input_tokens", 0),
            output_tokens=usage.get("output_tokens", 0),
            cost_usd=raw_data.get("total_cost_usd", 0.0) or 0.0,
            num_turns=raw_data.get("num_turns", 0) or 0,
            duration_ms=duration_ms,
            is_error=raw_data.get("is_error", False) or False,
            result_text=raw_data.get("result", "") or "",
        )
    )
