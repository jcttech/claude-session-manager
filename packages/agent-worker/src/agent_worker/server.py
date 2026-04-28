"""gRPC server for the Agent SDK worker."""

from __future__ import annotations

import argparse
import asyncio
import logging
import time

from claude_agent_sdk._internal.message_parser import parse_message
from claude_agent_sdk.types import (
    AssistantMessage,
    ResultMessage,
    StreamEvent,
    SystemMessage,
    UserMessage,
)
from grpc import aio as grpc_aio

from . import agent_pb2, agent_pb2_grpc
from .event_mapper import (
    error_event,
    extract_context_tokens_from_stream,
    fallback_result_event,
    map_assistant_message,
    map_result_message,
    map_stream_event,
    map_system_message,
)
from .session_manager import SessionManager

logger = logging.getLogger(__name__)

# Package version (set by hatch-vcs at build time, fallback for dev)
try:
    from importlib.metadata import version as pkg_version

    __version__ = pkg_version("agent-worker")
except Exception:
    __version__ = "0.0.0-dev"


class AgentWorkerServicer(agent_pb2_grpc.AgentWorkerServicer):
    """gRPC servicer that wraps the Claude Agent SDK."""

    def __init__(self) -> None:
        self.sessions = SessionManager()

    def _parse_and_map_event(self, raw_data, turn_start: float, context_tokens: int = 0):
        """Parse a raw SDK message dict and map it to an AgentEvent (or None).

        Returns (event_or_none, is_result, updated_context_tokens) where:
        - event_or_none: AgentEvent, list of AgentEvents, or None
        - is_result: True if a ResultMessage was processed (turn boundary)
        - updated_context_tokens: last known per-call input tokens (context window)
        """
        try:
            message = parse_message(raw_data)
        except Exception as parse_exc:
            exc_str = str(parse_exc)
            if "rate_limit_event" in exc_str:
                logger.info("Rate limit event received: %s", parse_exc)
                if isinstance(raw_data, dict):
                    logger.info("Rate limit details: %s", raw_data)
            elif isinstance(raw_data, dict) and raw_data.get("type") == "result":
                logger.error(
                    "ResultMessage parse failed — forcing exit: %s",
                    parse_exc,
                )
                evt = fallback_result_event(raw_data, turn_start, context_tokens)
                return evt, True, context_tokens
            else:
                logger.warning("Skipping unparseable message: %s", parse_exc)
            return None, False, context_tokens

        msg_type = type(message).__name__
        if isinstance(message, SystemMessage):
            logger.debug("received SystemMessage (subtype=%s)", message.subtype)
            event = map_system_message(message)
            return event, False, context_tokens

        elif isinstance(message, AssistantMessage):
            block_count = len(message.content) if hasattr(message, 'content') else 0
            logger.debug("received AssistantMessage (%d blocks)", block_count)
            # Return list of non-text events (tool_use, tool_result)
            events = [
                ev for ev in map_assistant_message(message)
                if not ev.HasField("text")
            ]
            return events, False, context_tokens

        elif isinstance(message, ResultMessage):
            logger.info("received ResultMessage (session=%s, turns=%s)",
                        getattr(message, 'session_id', '?'),
                        getattr(message, 'num_turns', '?'))
            return map_result_message(message, turn_start, context_tokens), True, context_tokens

        elif isinstance(message, StreamEvent):
            raw = message.event or {}
            event_type = raw.get("type", "unknown")
            logger.debug("received StreamEvent (%s)", event_type)
            # Track per-API-call input tokens from message_start events
            ctx = extract_context_tokens_from_stream(message)
            if ctx is not None:
                context_tokens = ctx
                logger.debug("Context tokens updated: %d", context_tokens)
            event = map_stream_event(message)
            return event, False, context_tokens

        elif isinstance(message, UserMessage):
            return None, False, context_tokens  # Tool results flowing back — not surfaced to chat

        else:
            logger.warning("Unhandled message type: %s", msg_type)
            return None, False, context_tokens

    async def Session(self, request_iterator, context):
        """Bidirectional streaming: one gRPC stream = one SDK session lifecycle.

        Uses concurrent tasks so that SDK events are read continuously,
        even between turns (e.g. background task completions).
        """
        await context.send_initial_metadata(())

        # Yield handshake event to flush gRPC response headers.
        logger.debug("Session: yielding handshake event to flush headers")
        yield agent_pb2.AgentEvent()

        client = None
        session_id = None
        events_yielded = 0
        msg_stream = None
        event_queue: asyncio.Queue = asyncio.Queue()
        # Shared mutable turn_start timestamp (updated by request_handler)
        turn_start = time.monotonic()

        async def sdk_reader():
            """Continuously read from SDK message stream and push events to queue."""
            nonlocal turn_start
            # Track per-API-call context tokens (last message_start usage)
            ctx_tokens = 0
            try:
                while True:
                    try:
                        raw_data = await msg_stream.__anext__()
                    except StopAsyncIteration:
                        logger.error(
                            "SDK iterator ended without ResultMessage"
                            " — pushing fallback error"
                        )
                        await event_queue.put(error_event(
                            "SDK message stream ended unexpectedly",
                            "iterator_exhausted",
                        ))
                        await event_queue.put(None)  # Sentinel: stream ended
                        return

                    event_or_list, _is_result, ctx_tokens = self._parse_and_map_event(
                        raw_data, turn_start, ctx_tokens,
                    )

                    if event_or_list is None:
                        continue
                    if isinstance(event_or_list, list):
                        for ev in event_or_list:
                            await event_queue.put(ev)
                    else:
                        await event_queue.put(event_or_list)
            except asyncio.CancelledError:
                return
            except Exception as exc:
                logger.exception("sdk_reader failed")
                await event_queue.put(error_event(str(exc), "sdk_reader_error"))
                await event_queue.put(None)

        async def request_handler():
            """Read gRPC requests and dispatch to SDK client."""
            nonlocal client, session_id, msg_stream, turn_start
            try:
                async for request in request_iterator:
                    input_type = request.WhichOneof("input")
                    turn_start = time.monotonic()

                    if input_type == "create":
                        logger.info("Session: create prompt=%s...",
                                    request.create.prompt[:80])
                        req = request.create
                        client = await self.sessions.create_session(
                            req.prompt,
                            permission_mode=req.permission_mode,
                            env=dict(req.env),
                            system_prompt_append=req.system_prompt_append,
                            max_turns=req.max_turns or None,
                            max_thinking_tokens=req.max_thinking_tokens or None,
                            resume_session_id=req.resume_session_id or None,
                        )
                        msg_stream = client._query.receive_messages().__aiter__()

                    elif input_type == "follow_up":
                        logger.info("Session: follow_up prompt=%s...",
                                    request.follow_up.prompt[:80])
                        if client is None:
                            await event_queue.put(
                                error_event("No session created yet", "no_session")
                            )
                            continue
                        await client.query(request.follow_up.prompt)
            except asyncio.CancelledError:
                return
            except Exception as exc:
                logger.exception("request_handler failed")
                await event_queue.put(error_event(str(exc), "request_handler_error"))
            finally:
                # Request stream ended — signal the main loop
                await event_queue.put(None)

        reader_task = None
        handler_task = None
        try:
            # Start request_handler immediately. sdk_reader starts once msg_stream exists.
            handler_task = asyncio.create_task(request_handler())

            # Wait for msg_stream to be created by request_handler (create request)
            while msg_stream is None:
                if handler_task.done():
                    break
                await asyncio.sleep(0.01)

            if msg_stream is not None:
                reader_task = asyncio.create_task(sdk_reader())

            # Main generator: yield events from queue until sentinel
            while True:
                event = await event_queue.get()
                if event is None:
                    # Sentinel: either request stream ended or SDK stream ended.
                    # If reader is still running, keep going (request stream ended,
                    # but SDK may still have background events).
                    if reader_task is not None and not reader_task.done():
                        # Request stream ended but SDK reader still active.
                        # This means gRPC client disconnected — stop.
                        break
                    break
                if hasattr(event, 'HasField') and event.HasField("session_init"):
                    session_id = event.session_init.session_id
                    if client is not None:
                        self.sessions.register(session_id, client)
                events_yielded += 1
                yield event

        except GeneratorExit:
            pass
        except Exception as exc:
            logger.exception("Session failed")
            yield error_event(str(exc), "session_error")
        finally:
            # Cancel both tasks on cleanup
            if reader_task is not None and not reader_task.done():
                reader_task.cancel()
                try:
                    await reader_task
                except asyncio.CancelledError:
                    pass
            if handler_task is not None and not handler_task.done():
                handler_task.cancel()
                try:
                    await handler_task
                except asyncio.CancelledError:
                    pass

        logger.info("Session: ended, yielded %d events", events_yielded)
        if session_id:
            await self.sessions.remove(session_id)

    async def Interrupt(self, request, context):
        """Interrupt a running session."""
        logger.info("Interrupt: session=%s", request.session_id)
        success = await self.sessions.interrupt(request.session_id)
        return agent_pb2.InterruptResponse(success=success)

    async def ClearSession(self, request, context):
        """Disconnect the SDK client for a session.

        The bidi Session task's sdk_reader will see StopAsyncIteration on the
        old message stream and shut down, ending the bidi stream. session-manager
        observes the disconnect and reconnects on the next user message — with
        is_first_message=true and no resume_session_id (caller's responsibility),
        the new stream starts a fresh conversation. This is how /clear actually
        works in SDK mode (slash commands aren't honored by the SDK).
        """
        logger.info("ClearSession: session=%s", request.session_id)
        existed = await self.sessions.remove(request.session_id)
        return agent_pb2.ClearResponse(success=existed)

    async def Health(self, request, context):
        """Health check endpoint."""
        return agent_pb2.HealthResponse(
            ready=True,
            worker_version=__version__,
        )


async def serve(port: int) -> None:
    """Start the gRPC server."""
    server = grpc_aio.server()
    servicer = AgentWorkerServicer()
    agent_pb2_grpc.add_AgentWorkerServicer_to_server(servicer, server)

    listen_addr = f"[::]:{port}"
    server.add_insecure_port(listen_addr)

    logger.info("Agent worker starting on %s (version %s)", listen_addr, __version__)
    await server.start()

    try:
        await server.wait_for_termination()
    except asyncio.CancelledError:
        logger.info("Shutting down...")
        await servicer.sessions.shutdown()
        await server.stop(grace=5)


def main() -> None:
    """CLI entry point."""
    parser = argparse.ArgumentParser(description="Agent SDK gRPC worker")
    parser.add_argument("--port", type=int, default=50051, help="Port to listen on")
    parser.add_argument(
        "--log-level",
        default="INFO",
        choices=["DEBUG", "INFO", "WARNING", "ERROR"],
        help="Log level",
    )
    args = parser.parse_args()

    logging.basicConfig(
        level=getattr(logging, args.log_level),
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
    )

    asyncio.run(serve(args.port))


if __name__ == "__main__":
    main()
