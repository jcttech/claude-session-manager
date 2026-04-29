"""gRPC server for the Agent SDK worker."""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import os
import time
import uuid

from claude_agent_sdk._internal.message_parser import parse_message
from claude_agent_sdk.types import (
    AssistantMessage,
    RateLimitEvent,
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
    map_rate_limit_event,
    map_result_message,
    map_stream_event,
    map_system_message,
    raw_error_event,
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
            if isinstance(raw_data, dict) and raw_data.get("type") == "result":
                logger.error(
                    "ResultMessage parse failed — forcing exit: %s",
                    parse_exc,
                )
                evt = fallback_result_event(raw_data, turn_start, context_tokens)
                return evt, True, context_tokens
            # Anything we can't parse — including malformed rate_limit_event
            # rows — is forwarded as a raw_json AgentError so session-manager
            # can surface it instead of silently dropping the signal.
            logger.warning("Forwarding unparseable SDK message: %s", parse_exc)
            evt = raw_error_event(
                raw_data,
                f"unparseable SDK message: {exc_str}",
                error_type="parse_failed",
            )
            return evt, False, context_tokens

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

        elif isinstance(message, RateLimitEvent):
            info = message.rate_limit_info
            logger.info(
                "RateLimitEvent: status=%s type=%s resets_at=%s utilization=%s",
                info.status,
                info.rate_limit_type,
                info.resets_at,
                info.utilization,
            )
            return map_rate_limit_event(message), False, context_tokens

        elif isinstance(message, UserMessage):
            return None, False, context_tokens  # Tool results flowing back — not surfaced to chat

        else:
            # Forward unknown SDK message types upstream as raw errors so we
            # can iterate without first round-tripping through a proto change.
            logger.warning("Forwarding unhandled SDK message type: %s", msg_type)
            evt = raw_error_event(
                raw_data,
                f"unhandled SDK message type: {msg_type}",
                error_type="unhandled_type",
            )
            return evt, False, context_tokens

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

        # `team` CLI correlation. Each in-container CLI invocation gets a
        # cli_id; we push CliCommand onto event_queue (forwarded upstream),
        # then the host's CliResponse arrives via request_handler and we
        # resolve the matching Future. Scoped to this Session task group, so
        # there's no cross-session leakage by construction.
        pending_cli: dict[str, asyncio.Future] = {}
        # Per-session Unix socket path. Populated when CreateSession.env
        # carries CSM_CLI_SOCKET; empty disables the CLI socket entirely
        # (e.g. legacy callers).
        cli_socket_path: str | None = None
        cli_socket_server: asyncio.AbstractServer | None = None
        cli_socket_task: asyncio.Task | None = None

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

        async def handle_cli_socket(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
            """Per-connection handler for the in-container `team` CLI.

            Wire format (one JSON object per line):
              request:  {"verb": "...", "args": [...], "flags": {...}}
              response: {"exit_code": N, "stdout": "...", "stderr": "..."}

            We allocate cli_id here so the CLI doesn't need to. Pushes a
            CliCommand onto the bidi event_queue and awaits the matching
            CliResponse from request_handler.
            """
            try:
                line = await asyncio.wait_for(reader.readline(), timeout=5.0)
                if not line:
                    return
                try:
                    payload = json.loads(line.decode("utf-8"))
                except json.JSONDecodeError as e:
                    err = json.dumps({"exit_code": 2, "stdout": "",
                                      "stderr": f"team: malformed request: {e}\n"}) + "\n"
                    writer.write(err.encode("utf-8"))
                    await writer.drain()
                    return

                cli_id = uuid.uuid4().hex
                fut: asyncio.Future = asyncio.get_event_loop().create_future()
                pending_cli[cli_id] = fut

                cmd = agent_pb2.CliCommand(
                    cli_id=cli_id,
                    verb=str(payload.get("verb", "")),
                    args=[str(a) for a in payload.get("args", []) or []],
                    flags={str(k): str(v) for k, v in (payload.get("flags") or {}).items()},
                )
                await event_queue.put(agent_pb2.AgentEvent(cli_command=cmd))

                try:
                    resp = await asyncio.wait_for(fut, timeout=30.0)
                    body = json.dumps({
                        "exit_code": int(resp.exit_code),
                        "stdout": resp.stdout,
                        "stderr": resp.stderr,
                    }) + "\n"
                except asyncio.TimeoutError:
                    body = json.dumps({
                        "exit_code": 124,
                        "stdout": "",
                        "stderr": "team: timed out waiting for session-manager response\n",
                    }) + "\n"
                finally:
                    pending_cli.pop(cli_id, None)

                writer.write(body.encode("utf-8"))
                await writer.drain()
            except asyncio.CancelledError:
                raise
            except Exception:
                logger.exception("CLI socket handler failed")
            finally:
                try:
                    writer.close()
                    await writer.wait_closed()
                except Exception:
                    pass

        async def start_cli_socket(path: str):
            """Bind the per-session Unix socket. Idempotent: removes a stale
            file from a prior crash before binding."""
            nonlocal cli_socket_server
            try:
                os.unlink(path)
            except FileNotFoundError:
                pass
            except OSError as e:
                logger.warning("CLI socket unlink failed for %s: %s", path, e)
            cli_socket_server = await asyncio.start_unix_server(handle_cli_socket, path=path)
            try:
                os.chmod(path, 0o600)
            except OSError:
                pass
            logger.info("CLI socket listening on %s", path)

        async def request_handler():
            """Read gRPC requests and dispatch to SDK client."""
            nonlocal client, session_id, msg_stream, turn_start, cli_socket_path, cli_socket_task
            try:
                async for request in request_iterator:
                    input_type = request.WhichOneof("input")
                    turn_start = time.monotonic()

                    if input_type == "create":
                        logger.info("Session: create prompt=%s...",
                                    request.create.prompt[:80])
                        req = request.create
                        env_map = dict(req.env)
                        # Lift CSM_CLI_SOCKET out of the env map so we can bind
                        # the socket here, but still pass it through to the
                        # SDK subprocess so the LLM's Bash tool inherits it.
                        cli_sock = env_map.get("CSM_CLI_SOCKET", "").strip()
                        client = await self.sessions.create_session(
                            req.prompt,
                            permission_mode=req.permission_mode,
                            env=env_map,
                            system_prompt_append=req.system_prompt_append,
                            max_turns=req.max_turns or None,
                            max_thinking_tokens=req.max_thinking_tokens or None,
                            resume_session_id=req.resume_session_id or None,
                        )
                        msg_stream = client._query.receive_messages().__aiter__()
                        if cli_sock and cli_socket_task is None:
                            cli_socket_path = cli_sock
                            cli_socket_task = asyncio.create_task(start_cli_socket(cli_sock))

                    elif input_type == "follow_up":
                        logger.info("Session: follow_up prompt=%s...",
                                    request.follow_up.prompt[:80])
                        if client is None:
                            await event_queue.put(
                                error_event("No session created yet", "no_session")
                            )
                            continue
                        await client.query(request.follow_up.prompt)

                    elif input_type == "cli_response":
                        # Match the host's reply with the awaiting CLI handler.
                        resp = request.cli_response
                        fut = pending_cli.get(resp.cli_id)
                        if fut is not None and not fut.done():
                            fut.set_result(resp)
                        else:
                            logger.debug(
                                "CliResponse for unknown cli_id=%s (timed out?)",
                                resp.cli_id,
                            )
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
            # Cancel all task-group members on cleanup
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

            # Tear down the CLI socket. Cancel any pending CLI handlers so
            # the futures don't leak (each waiter sees CancelledError and
            # writes a structured error back to its own connected CLI).
            for fut in list(pending_cli.values()):
                if not fut.done():
                    fut.cancel()
            pending_cli.clear()
            if cli_socket_server is not None:
                cli_socket_server.close()
                try:
                    await cli_socket_server.wait_closed()
                except Exception:
                    pass
            if cli_socket_task is not None and not cli_socket_task.done():
                cli_socket_task.cancel()
                try:
                    await cli_socket_task
                except asyncio.CancelledError:
                    pass
            if cli_socket_path:
                try:
                    os.unlink(cli_socket_path)
                except FileNotFoundError:
                    pass
                except OSError as e:
                    logger.warning("CLI socket unlink on shutdown failed: %s", e)

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
