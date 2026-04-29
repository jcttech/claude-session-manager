"""Map Claude Agent SDK message types to protobuf AgentEvent messages."""

from __future__ import annotations

import json
import time
from typing import TYPE_CHECKING

from . import agent_pb2

if TYPE_CHECKING:
    from claude_agent_sdk.types import (
        AssistantMessage,
        RateLimitEvent,
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


def _total_input_tokens(usage: dict) -> int:
    """Compute total context size including cached tokens.

    The Claude API splits input tokens across three fields when prompt caching
    is active: input_tokens (non-cached), cache_read_input_tokens, and
    cache_creation_input_tokens. The sum represents the actual context window usage.
    """
    return (
        usage.get("input_tokens", 0)
        + usage.get("cache_read_input_tokens", 0)
        + usage.get("cache_creation_input_tokens", 0)
    )


def map_result_message(
    msg: ResultMessage, start_time: float, context_tokens: int = 0,
) -> agent_pb2.AgentEvent:
    """Map a ResultMessage to a SessionResult AgentEvent.

    Args:
        msg: The ResultMessage from the Claude Agent SDK.
        start_time: Monotonic timestamp when the turn started.
        context_tokens: Input tokens from the last API call (actual context
            window usage). When 0, falls back to the cumulative usage total.
    """
    usage = msg.usage or {}
    duration_ms = int((time.monotonic() - start_time) * 1000)
    cumulative_input = _total_input_tokens(usage)

    return agent_pb2.AgentEvent(
        result=agent_pb2.SessionResult(
            session_id=msg.session_id or "",
            input_tokens=cumulative_input,
            output_tokens=usage.get("output_tokens", 0),
            cost_usd=msg.total_cost_usd or 0.0,
            num_turns=msg.num_turns or 0,
            duration_ms=duration_ms,
            is_error=msg.is_error or False,
            result_text=msg.result or "",
            context_tokens=context_tokens if context_tokens > 0 else cumulative_input,
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


def extract_context_tokens_from_stream(event: StreamEvent) -> int | None:
    """Extract per-API-call input tokens from a message_start stream event.

    The Anthropic API emits a message_start event at the beginning of each API
    call with `usage.input_tokens` representing the context window consumed by
    that single call. This is distinct from the cumulative usage reported in
    the final ResultMessage.

    Returns the input token count if this is a message_start event with usage,
    otherwise None.
    """
    raw = event.event or {}
    if raw.get("type") != "message_start":
        return None
    message = raw.get("message", {})
    usage = message.get("usage", {})
    if not usage:
        return None
    return _total_input_tokens(usage)


def error_event(
    message: str,
    error_type: str = "internal",
    raw_json: str = "",
) -> agent_pb2.AgentEvent:
    """Create an AgentError event.

    `raw_json` carries the original SDK payload when we couldn't parse it —
    session-manager logs it so we can iterate on unmodeled signals.
    """
    return agent_pb2.AgentEvent(
        error=agent_pb2.AgentError(
            message=message,
            error_type=error_type,
            raw_json=raw_json,
        )
    )


def raw_error_event(
    raw_data: object,
    reason: str,
    error_type: str = "unparseable",
) -> agent_pb2.AgentEvent:
    """Wrap an unrecognised / unparseable SDK payload as an AgentError.

    We never silently drop a message: even if we don't know what to do with
    it, we forward the raw bytes upstream so it shows up in logs / Mattermost
    cards and we can decide whether to bake in a typed handler later.
    """
    try:
        raw_json = json.dumps(raw_data, default=str)
    except (TypeError, ValueError):
        raw_json = repr(raw_data)
    return agent_pb2.AgentEvent(
        error=agent_pb2.AgentError(
            message=reason,
            error_type=error_type,
            raw_json=raw_json,
        )
    )


def map_rate_limit_event(event: RateLimitEvent) -> agent_pb2.AgentEvent:
    """Map a RateLimitEvent to a RateLimit AgentEvent.

    The SDK emits these whenever the rate-limit status transitions. We forward
    everything (including the raw dict) so session-manager can pause / resume
    the team task queue.
    """
    info = event.rate_limit_info
    try:
        raw_json = json.dumps(info.raw, default=str)
    except (TypeError, ValueError):
        raw_json = "{}"
    return agent_pb2.AgentEvent(
        rate_limit=agent_pb2.RateLimit(
            status=info.status or "",
            resets_at=info.resets_at or 0,
            limit_type=info.rate_limit_type or "",
            utilization=info.utilization if info.utilization is not None else -1.0,
            raw_json=raw_json,
        )
    )


def fallback_result_event(
    raw_data: dict, start_time: float, context_tokens: int = 0,
) -> agent_pb2.AgentEvent:
    """Build a SessionResult from raw dict when ResultMessage fails to parse.

    Uses .get() with safe defaults for all fields so partial data doesn't crash.
    """
    usage = raw_data.get("usage", {}) or {}
    duration_ms = int((time.monotonic() - start_time) * 1000)
    cumulative_input = _total_input_tokens(usage)

    return agent_pb2.AgentEvent(
        result=agent_pb2.SessionResult(
            session_id=raw_data.get("session_id", ""),
            input_tokens=cumulative_input,
            output_tokens=usage.get("output_tokens", 0),
            cost_usd=raw_data.get("total_cost_usd", 0.0) or 0.0,
            num_turns=raw_data.get("num_turns", 0) or 0,
            duration_ms=duration_ms,
            is_error=raw_data.get("is_error", False) or False,
            result_text=raw_data.get("result", "") or "",
            context_tokens=context_tokens if context_tokens > 0 else cumulative_input,
        )
    )
