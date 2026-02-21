"""Standalone test client for the agent worker gRPC server.

Usage:
    python -m tests.test_client --port 50051 --prompt "What is 2+2?"

This opens a bidirectional Session stream, sends a CreateSession request,
prints all events, then sends a FollowUp message and prints those events.
Useful for manual validation of the worker before Rust integration.
"""

from __future__ import annotations

import argparse
import asyncio
import sys
from pathlib import Path

from grpc import aio as grpc_aio

# Ensure generated proto stubs are importable
sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

from agent_worker import agent_pb2, agent_pb2_grpc  # noqa: E402


async def request_generator(prompt: str, session_id: str | None):
    """Generate SessionInput messages for the bidirectional stream."""
    # First: CreateSession
    yield agent_pb2.SessionInput(
        create=agent_pb2.CreateSession(
            prompt=prompt,
            permission_mode="bypassPermissions",
        )
    )

    # Wait a bit for the first turn to complete, then send follow-up
    # In practice, the Rust side coordinates this based on SessionResult events
    await asyncio.sleep(2)

    if session_id:
        yield agent_pb2.SessionInput(
            follow_up=agent_pb2.FollowUp(
                prompt="What did I just ask you?",
            )
        )


async def run_test(port: int, prompt: str) -> None:
    """Execute a test prompt and print all received events."""
    addr = f"localhost:{port}"
    print(f"Connecting to {addr}...")

    async with grpc_aio.insecure_channel(addr) as channel:
        stub = agent_pb2_grpc.AgentWorkerStub(channel)

        # Health check
        print("Checking health...")
        health = await stub.Health(agent_pb2.HealthRequest())
        print(f"  ready={health.ready}, version={health.worker_version}")

        if not health.ready:
            print("Worker not ready, aborting.")
            return

        # Session (bidirectional stream)
        print(f"\nStarting session with: {prompt!r}")
        print("-" * 60)

        # We need to coordinate sending requests with receiving events.
        # Use a queue to send requests from the event processing loop.
        request_queue = asyncio.Queue()

        # Send initial CreateSession
        await request_queue.put(agent_pb2.SessionInput(
            create=agent_pb2.CreateSession(
                prompt=prompt,
                permission_mode="bypassPermissions",
            )
        ))

        async def request_iter():
            while True:
                req = await request_queue.get()
                if req is None:
                    break
                yield req

        session_id = None
        turn = 0

        async for event in stub.Session(request_iter()):
            field = event.WhichOneof("event")

            if field == "session_init":
                session_id = event.session_init.session_id
                print(f"[SessionInit] session_id={session_id}")

            elif field == "text":
                partial = " (partial)" if event.text.is_partial else ""
                print(f"[Text{partial}] {event.text.text}")

            elif field == "tool_use":
                print(f"[ToolUse] {event.tool_use.tool_name} (id={event.tool_use.tool_use_id})")

            elif field == "tool_result":
                error = " ERROR" if event.tool_result.is_error else ""
                print(f"[ToolResult{error}] id={event.tool_result.tool_use_id}")

            elif field == "subagent":
                action = "START" if event.subagent.is_start else "END"
                print(f"[Subagent {action}] {event.subagent.agent_name}")

            elif field == "result":
                r = event.result
                print(f"\n[Result] session={r.session_id}")
                print(f"  tokens: in={r.input_tokens} out={r.output_tokens}")
                print(f"  cost: ${r.cost_usd:.4f}")
                print(f"  turns: {r.num_turns}")
                print(f"  duration: {r.duration_ms}ms")
                print(f"  error: {r.is_error}")

                turn += 1
                if turn == 1 and session_id:
                    # Send follow-up after first turn
                    print(f"\n{'=' * 60}")
                    print("Sending FollowUp...")
                    print("-" * 60)
                    await request_queue.put(agent_pb2.SessionInput(
                        follow_up=agent_pb2.FollowUp(
                            prompt="What did I just ask you?",
                        )
                    ))
                else:
                    # Done — close the request stream
                    await request_queue.put(None)

            elif field == "error":
                print(f"[ERROR] {event.error.error_type}: {event.error.message}")

        print("\nDone.")


def main() -> None:
    parser = argparse.ArgumentParser(description="Test the agent worker gRPC server")
    parser.add_argument("--port", type=int, default=50051)
    parser.add_argument("--prompt", default="What is 2+2? Reply with just the number.")
    args = parser.parse_args()
    asyncio.run(run_test(args.port, args.prompt))


if __name__ == "__main__":
    main()
