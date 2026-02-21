"""Standalone test client for the agent worker gRPC server.

Usage:
    python -m tests.test_client --port 50051 --prompt "What is 2+2?"

This sends a single Execute request and prints all events received.
Useful for manual validation of the worker before Rust integration.
"""

from __future__ import annotations

import argparse
import asyncio
import sys
from pathlib import Path

import grpc
from grpc import aio as grpc_aio

# Ensure generated proto stubs are importable
sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

from agent_worker import agent_pb2, agent_pb2_grpc  # noqa: E402


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

        # Execute
        print(f"\nExecuting: {prompt!r}")
        print("-" * 60)

        request = agent_pb2.ExecuteRequest(
            prompt=prompt,
            permission_mode="bypassPermissions",
        )

        session_id = None
        async for event in stub.Execute(request):
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

            elif field == "error":
                print(f"[ERROR] {event.error.error_type}: {event.error.message}")

        # Test SendMessage if we got a session
        if session_id:
            print(f"\n{'=' * 60}")
            print(f"Testing SendMessage with session_id={session_id}")
            print("-" * 60)

            send_request = agent_pb2.SendMessageRequest(
                session_id=session_id,
                prompt="What did I just ask you?",
            )
            async for event in stub.SendMessage(send_request):
                field = event.WhichOneof("event")
                if field == "text" and not event.text.is_partial:
                    print(f"[Text] {event.text.text}")
                elif field == "result":
                    print(f"[Result] turns={event.result.num_turns}")
                elif field == "error":
                    print(f"[ERROR] {event.error.message}")

        print("\nDone.")


def main() -> None:
    parser = argparse.ArgumentParser(description="Test the agent worker gRPC server")
    parser.add_argument("--port", type=int, default=50051)
    parser.add_argument("--prompt", default="What is 2+2? Reply with just the number.")
    args = parser.parse_args()
    asyncio.run(run_test(args.port, args.prompt))


if __name__ == "__main__":
    main()
