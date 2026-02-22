"""gRPC server for the Agent SDK worker."""

from __future__ import annotations

import argparse
import asyncio
import logging
import time

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

    async def _iter_sdk_messages(self, messages, start_time: float):
        """Shared async generator that iterates SDK messages and yields AgentEvents.

        Handles all message types (SystemMessage, AssistantMessage, ResultMessage,
        StreamEvent, UserMessage) and the parse-failure fix for ResultMessage.
        """
        msg_iter = messages.__aiter__()
        while True:
            try:
                message = await msg_iter.__anext__()
            except StopAsyncIteration:
                break
            except Exception as parse_exc:
                logger.warning("Skipping unparseable message: %s", parse_exc)
                # Check if this is a ResultMessage parse failure — if so, force exit
                try:
                    from claude_agent_sdk._errors import MessageParseError
                    raw_data = getattr(parse_exc, 'data', None)
                    if isinstance(parse_exc, MessageParseError) and isinstance(raw_data, dict):
                        if raw_data.get("type") == "result":
                            logger.error(
                                "ResultMessage parse failed — forcing exit: %s",
                                parse_exc,
                            )
                            yield fallback_result_event(raw_data, start_time)
                            return
                except ImportError:
                    pass
                continue

            msg_type = type(message).__name__
            if isinstance(message, SystemMessage):
                logger.debug("received SystemMessage (subtype=%s)", message.subtype)
                event = map_system_message(message)
                if event is not None:
                    yield event

            elif isinstance(message, AssistantMessage):
                block_count = len(message.content) if hasattr(message, 'content') else 0
                logger.debug("received AssistantMessage (%d blocks)", block_count)
                for event in map_assistant_message(message):
                    yield event

            elif isinstance(message, ResultMessage):
                logger.info("received ResultMessage (session=%s, turns=%s)",
                            getattr(message, 'session_id', '?'),
                            getattr(message, 'num_turns', '?'))
                yield map_result_message(message, start_time)
                return  # ResultMessage terminates the turn

            elif isinstance(message, StreamEvent):
                raw = message.event or {}
                event_type = raw.get("type", "unknown")
                logger.debug("received StreamEvent (%s)", event_type)
                event = map_stream_event(message)
                if event is not None:
                    yield event

            elif isinstance(message, UserMessage):
                pass  # Tool results flowing back — not surfaced to chat

            else:
                logger.warning("Unhandled message type: %s", msg_type)

    async def Session(self, request_iterator, context):
        """Bidirectional streaming: one gRPC stream = one SDK session lifecycle."""
        # Send initial metadata immediately so the client's session() call
        # doesn't block waiting for response headers (avoids deadlock).
        await context.send_initial_metadata(())

        # grpcio buffers response headers until the first message is yielded.
        # Yield an empty AgentEvent to force header flush over the wire.
        # The Rust client skips events with no variant (process_turn_events line 145-148).
        logger.debug("Session: yielding handshake event to flush headers")
        yield agent_pb2.AgentEvent()

        client = None
        session_id = None
        events_yielded = 0

        try:
            async for request in request_iterator:
                input_type = request.WhichOneof("input")
                turn_start = time.monotonic()

                if input_type == "create":
                    # First message: create SDK client
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
                    )
                    msgs = client.receive_messages()
                    async for event in self._iter_sdk_messages(msgs, turn_start):
                        events_yielded += 1
                        if event.HasField("session_init"):
                            session_id = event.session_init.session_id
                            self.sessions.register(session_id, client)
                        yield event
                        if event.HasField("result"):
                            break

                elif input_type == "follow_up":
                    logger.info("Session: follow_up prompt=%s...",
                                request.follow_up.prompt[:80])
                    if client is None:
                        yield error_event("No session created yet", "no_session")
                        continue
                    await client.query(request.follow_up.prompt)
                    msgs = client.receive_response()
                    async for event in self._iter_sdk_messages(msgs, turn_start):
                        events_yielded += 1
                        yield event
                        if event.HasField("result"):
                            break

        except GeneratorExit:
            pass
        except Exception as exc:
            logger.exception("Session failed")
            yield error_event(str(exc), "session_error")

        logger.info("Session: ended, yielded %d events", events_yielded)
        if session_id:
            await self.sessions.remove(session_id)

    async def Interrupt(self, request, context):
        """Interrupt a running session."""
        logger.info("Interrupt: session=%s", request.session_id)
        success = await self.sessions.interrupt(request.session_id)
        return agent_pb2.InterruptResponse(success=success)

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
