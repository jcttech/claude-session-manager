"""Tests for the event_mapper module."""

from __future__ import annotations

import sys
from dataclasses import dataclass, field
from pathlib import Path
from unittest.mock import patch

import pytest

# Ensure generated proto stubs are importable
sys.path.insert(0, str(Path(__file__).parent.parent / "src"))


# --- Mock SDK types for unit testing (avoids requiring claude-agent-sdk installed) ---


@dataclass
class TextBlock:
    text: str


@dataclass
class ToolUseBlock:
    id: str
    name: str
    input: dict


@dataclass
class ToolResultBlock:
    tool_use_id: str
    content: str | None = None
    is_error: bool | None = None


@dataclass
class SystemMessage:
    subtype: str
    data: dict = field(default_factory=dict)


@dataclass
class AssistantMessage:
    content: list = field(default_factory=list)
    model: str = "claude-opus-4-6"


@dataclass
class ResultMessage:
    subtype: str = "success"
    duration_ms: int = 1000
    duration_api_ms: int = 800
    is_error: bool = False
    num_turns: int = 1
    session_id: str = "test-session"
    total_cost_usd: float | None = 0.05
    usage: dict | None = None
    result: str | None = "Done"
    structured_output: object = None


@dataclass
class StreamEvent:
    uuid: str = "evt-1"
    session_id: str = "test-session"
    event: dict = field(default_factory=dict)
    parent_tool_use_id: str | None = None


# Patch claude_agent_sdk module with our mocks before importing event_mapper
mock_sdk = type(sys)("claude_agent_sdk")
mock_sdk.TextBlock = TextBlock
mock_sdk.ToolUseBlock = ToolUseBlock
mock_sdk.ToolResultBlock = ToolResultBlock
mock_sdk.SystemMessage = SystemMessage
mock_sdk.AssistantMessage = AssistantMessage
mock_sdk.ResultMessage = ResultMessage
mock_sdk.StreamEvent = StreamEvent
sys.modules["claude_agent_sdk"] = mock_sdk

from agent_worker.event_mapper import (  # noqa: E402
    error_event,
    map_assistant_message,
    map_result_message,
    map_stream_event,
    map_system_message,
)


class TestMapSystemMessage:
    def test_init_event(self):
        msg = SystemMessage(subtype="init", data={"session_id": "abc-123"})
        event = map_system_message(msg)
        assert event is not None
        assert event.HasField("session_init")
        assert event.session_init.session_id == "abc-123"

    def test_non_init_event(self):
        msg = SystemMessage(subtype="hook_started", data={})
        event = map_system_message(msg)
        assert event is None

    def test_init_without_session_id(self):
        msg = SystemMessage(subtype="init", data={})
        event = map_system_message(msg)
        assert event is not None
        assert event.session_init.session_id == ""


class TestMapAssistantMessage:
    def test_text_block(self):
        msg = AssistantMessage(content=[TextBlock(text="Hello world")])
        events = map_assistant_message(msg)
        assert len(events) == 1
        assert events[0].HasField("text")
        assert events[0].text.text == "Hello world"
        assert events[0].text.is_partial is False

    def test_tool_use_block(self):
        msg = AssistantMessage(
            content=[
                ToolUseBlock(
                    id="tool-1",
                    name="Read",
                    input={"file_path": "/src/main.rs"},
                )
            ]
        )
        events = map_assistant_message(msg)
        assert len(events) == 1
        assert events[0].HasField("tool_use")
        assert events[0].tool_use.tool_name == "Read"
        assert events[0].tool_use.tool_use_id == "tool-1"
        assert '"file_path"' in events[0].tool_use.input_json

    def test_tool_result_block(self):
        msg = AssistantMessage(
            content=[ToolResultBlock(tool_use_id="tool-1", is_error=True)]
        )
        events = map_assistant_message(msg)
        assert len(events) == 1
        assert events[0].HasField("tool_result")
        assert events[0].tool_result.tool_use_id == "tool-1"
        assert events[0].tool_result.is_error is True

    def test_mixed_content(self):
        msg = AssistantMessage(
            content=[
                TextBlock(text="Let me read the file."),
                ToolUseBlock(id="t1", name="Read", input={"file_path": "/x"}),
                ToolResultBlock(tool_use_id="t1", is_error=False),
                TextBlock(text="Here is the content."),
            ]
        )
        events = map_assistant_message(msg)
        assert len(events) == 4
        assert events[0].HasField("text")
        assert events[1].HasField("tool_use")
        assert events[2].HasField("tool_result")
        assert events[3].HasField("text")

    def test_empty_content(self):
        msg = AssistantMessage(content=[])
        events = map_assistant_message(msg)
        assert len(events) == 0


class TestMapResultMessage:
    def test_success_result(self):
        msg = ResultMessage(
            session_id="sess-1",
            usage={"input_tokens": 1000, "output_tokens": 500},
            total_cost_usd=0.05,
            num_turns=3,
            is_error=False,
            result="All done",
        )
        event = map_result_message(msg, start_time=0)
        assert event.HasField("result")
        assert event.result.session_id == "sess-1"
        assert event.result.input_tokens == 1000
        assert event.result.output_tokens == 500
        assert event.result.cost_usd == pytest.approx(0.05)
        assert event.result.num_turns == 3
        assert event.result.is_error is False
        assert event.result.result_text == "All done"
        assert event.result.duration_ms > 0

    def test_error_result(self):
        msg = ResultMessage(
            session_id="sess-2",
            is_error=True,
            result="Something went wrong",
            usage=None,
            total_cost_usd=None,
            num_turns=0,
        )
        event = map_result_message(msg, start_time=0)
        assert event.result.is_error is True
        assert event.result.result_text == "Something went wrong"
        assert event.result.input_tokens == 0


class TestMapStreamEvent:
    def test_text_delta(self):
        event = StreamEvent(
            event={
                "type": "content_block_delta",
                "delta": {"type": "text_delta", "text": "Hello"},
            }
        )
        result = map_stream_event(event)
        assert result is not None
        assert result.HasField("text")
        assert result.text.text == "Hello"
        assert result.text.is_partial is True

    def test_subagent_start(self):
        event = StreamEvent(
            parent_tool_use_id="parent-1",
            event={
                "type": "content_block_start",
                "content_block": {"name": "code-reviewer"},
            },
        )
        result = map_stream_event(event)
        assert result is not None
        assert result.HasField("subagent")
        assert result.subagent.is_start is True
        assert result.subagent.agent_name == "code-reviewer"

    def test_subagent_end(self):
        event = StreamEvent(
            parent_tool_use_id="parent-1",
            event={"type": "message_stop"},
        )
        result = map_stream_event(event)
        assert result is not None
        assert result.HasField("subagent")
        assert result.subagent.is_start is False

    def test_irrelevant_event(self):
        event = StreamEvent(event={"type": "ping"})
        result = map_stream_event(event)
        assert result is None


class TestErrorEvent:
    def test_error_event(self):
        event = error_event("Something broke", "test_error")
        assert event.HasField("error")
        assert event.error.message == "Something broke"
        assert event.error.error_type == "test_error"

    def test_default_error_type(self):
        event = error_event("Oops")
        assert event.error.error_type == "internal"
