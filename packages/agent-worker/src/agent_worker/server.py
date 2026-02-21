"""gRPC server for the Agent SDK worker."""

from __future__ import annotations

import argparse
import asyncio
import logging
import time

from claude_agent_sdk import (
    AssistantMessage,
    ResultMessage,
    StreamEvent,
    SystemMessage,
)
from grpc import aio as grpc_aio

from . import agent_pb2, agent_pb2_grpc
from .event_mapper import (
    error_event,
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

    async def Execute(self, request, context):
        """Create a new session and stream events."""
        start_time = time.monotonic()
        logger.info("Execute: prompt=%s...", request.prompt[:80])

        try:
            client = await self.sessions.create_session(
                request.prompt,
                permission_mode=request.permission_mode,
                env=dict(request.env),
                system_prompt_append=request.system_prompt_append,
                max_turns=request.max_turns if request.max_turns > 0 else None,
            )

            session_id = None
            async for message in client.receive_messages():
                if isinstance(message, SystemMessage):
                    event = map_system_message(message)
                    if event is not None:
                        if event.HasField("session_init"):
                            session_id = event.session_init.session_id
                            self.sessions.register(session_id, client)
                        yield event

                elif isinstance(message, AssistantMessage):
                    for event in map_assistant_message(message):
                        yield event

                elif isinstance(message, ResultMessage):
                    yield map_result_message(message, start_time)
                    break

                elif isinstance(message, StreamEvent):
                    event = map_stream_event(message)
                    if event is not None:
                        yield event

        except Exception as exc:
            logger.exception("Execute failed")
            yield error_event(str(exc), "execute_error")

    async def SendMessage(self, request, context):
        """Send a follow-up message to an existing session."""
        start_time = time.monotonic()
        logger.info("SendMessage: session=%s", request.session_id)

        client = self.sessions.get(request.session_id)
        if client is None:
            yield error_event(
                f"Session not found: {request.session_id}",
                "session_not_found",
            )
            return

        try:
            await client.query(request.prompt)

            async for message in client.receive_response():
                if isinstance(message, AssistantMessage):
                    for event in map_assistant_message(message):
                        yield event

                elif isinstance(message, ResultMessage):
                    yield map_result_message(message, start_time)
                    break

                elif isinstance(message, StreamEvent):
                    event = map_stream_event(message)
                    if event is not None:
                        yield event

        except Exception as exc:
            logger.exception("SendMessage failed")
            yield error_event(str(exc), "send_message_error")

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
