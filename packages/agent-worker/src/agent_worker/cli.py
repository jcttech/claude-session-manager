"""`team` CLI binary — invoked by the LLM via the Bash tool.

Talks to the in-container agent-worker over a per-session Unix socket whose
path is passed in via the CSM_CLI_SOCKET environment variable. agent-worker
forwards the command upstream to session-manager via the existing bidi gRPC
Session stream and relays the response back.

Wire format (one JSON object per line, both directions):
    request:  {"verb": str, "args": [str, ...], "flags": {str: str}}
    response: {"exit_code": int, "stdout": str, "stderr": str}
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import sys
from typing import NoReturn


SOCKET_ENV_VAR = "CSM_CLI_SOCKET"
SOCKET_TIMEOUT_SECS = 35  # session-manager itself caps at 30s; give socket a little headroom


def _build_parser() -> argparse.ArgumentParser:
    """Top-level parser with one subcommand per verb.

    Verbs match the legacy markers 1:1: status / to / spawn / interrupt /
    cancel / clear. session-manager validates the verb again, so this is just
    a UX layer — unknown verbs are still allowed through (will exit non-zero).
    """
    parser = argparse.ArgumentParser(
        prog="team",
        description="Interact with the team task queue from inside a managed session.",
    )
    sub = parser.add_subparsers(dest="verb", required=True)

    sub_status = sub.add_parser("status", help="Show team roster + queue depth.")
    sub_status.add_argument(
        "--json",
        action="store_true",
        help="Emit structured JSON instead of human-readable text.",
    )
    sub_status.set_defaults(args=[])

    sub_to = sub.add_parser("to", help="Send a message to a team member by role.")
    sub_to.add_argument("role", help="Target role (e.g. 'Developer' or 'Developer 2').")
    sub_to.add_argument("message", help="The message body. Quote it.")

    sub_spawn = sub.add_parser("spawn", help="Spawn a new team member with an initial task.")
    sub_spawn.add_argument("role", help="Role to spawn (e.g. 'Architect').")
    sub_spawn.add_argument("task", help="Initial task text. Quote it.")

    sub_int = sub.add_parser("interrupt", help="Abort a member's in-flight turn.")
    sub_int.add_argument("role", help="Target role; prefix matches every member of that role.")

    sub_cancel = sub.add_parser("cancel", help="Cancel a queued (not-yet-running) task.")
    sub_cancel.add_argument(
        "queue_id",
        nargs="?",
        help="Specific queue_id to cancel. If omitted, cancels all queued from this sender.",
    )
    sub_cancel.add_argument("--role", help="Restrict to tasks targeting this role.")

    sub_clear = sub.add_parser("clear", help="Reset a member's context window.")
    sub_clear.add_argument("role", help="Role to clear ('Self' to clear yourself).")

    sub_broadcast = sub.add_parser(
        "broadcast",
        help="Send a message to every other active team member (Lead-only).",
    )
    sub_broadcast.add_argument("message", help="The message body. Quote it.")

    sub_pr_ready = sub.add_parser(
        "pr-ready",
        help="Members: signal a PR is ready for the Claude Reviewer CI gate.",
    )
    sub_pr_ready.add_argument("pr_number", help="The PR number (integer).")

    sub_pr_reviewed = sub.add_parser(
        "pr-reviewed",
        help="Members: confirm review comments are addressed; forward to the Lead.",
    )
    sub_pr_reviewed.add_argument("pr_number", help="The PR number (integer).")

    sub_pr_merged = sub.add_parser(
        "pr-merged",
        help="Lead: signal a PR was just merged; queues a self-clear at the workflow boundary.",
    )
    sub_pr_merged.add_argument("pr_number", help="The PR number that was merged.")

    return parser


def _request_payload(ns: argparse.Namespace) -> dict:
    """Translate parsed argparse namespace → wire JSON request.

    Verb-specific arg shapes are stored in `args` (positional, in declared
    order), with shared flags (--json, --role) lifted into `flags`. session-
    manager's dispatcher consumes the same shape.
    """
    flags: dict[str, str] = {}
    if getattr(ns, "json", False):
        flags["json"] = "1"

    args: list[str] = []
    if ns.verb == "status":
        pass
    elif ns.verb == "to":
        args = [ns.role, ns.message]
    elif ns.verb == "spawn":
        args = [ns.role, ns.task]
    elif ns.verb == "interrupt":
        args = [ns.role]
    elif ns.verb == "cancel":
        if ns.queue_id is not None:
            args = [ns.queue_id]
        if getattr(ns, "role", None):
            flags["role"] = ns.role
    elif ns.verb == "clear":
        args = [ns.role]
    elif ns.verb == "broadcast":
        args = [ns.message]
    elif ns.verb in ("pr-ready", "pr-reviewed", "pr-merged"):
        args = [ns.pr_number]

    return {"verb": ns.verb, "args": args, "flags": flags}


def _connect_and_exchange(socket_path: str, payload: dict) -> dict:
    """Round-trip one JSON request/response over the per-session Unix socket.

    Returns the parsed response dict, or raises with a useful message that
    `main` turns into a non-zero exit. We use blocking sockets here — the CLI
    is short-lived and async would be overkill.
    """
    if not os.path.exists(socket_path):
        raise RuntimeError(
            f"team: agent-worker socket missing at {socket_path}. "
            "Is the session still attached?"
        )

    line = (json.dumps(payload) + "\n").encode("utf-8")

    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(SOCKET_TIMEOUT_SECS)
    try:
        s.connect(socket_path)
    except (FileNotFoundError, ConnectionRefusedError) as e:
        raise RuntimeError(f"team: cannot connect to {socket_path}: {e}") from e

    with s:
        s.sendall(line)
        # Read until newline. Server sends exactly one JSON line + close.
        buf = bytearray()
        while True:
            chunk = s.recv(8192)
            if not chunk:
                break
            buf.extend(chunk)
            if b"\n" in buf:
                break
        if not buf:
            raise RuntimeError("team: empty response from agent-worker")

    raw = bytes(buf).split(b"\n", 1)[0].decode("utf-8")
    try:
        return json.loads(raw)
    except json.JSONDecodeError as e:
        raise RuntimeError(f"team: malformed response: {e}: {raw!r}") from e


def main(argv: list[str] | None = None) -> NoReturn:  # noqa: D401
    """Entry point registered as the `team` console script."""
    parser = _build_parser()
    ns = parser.parse_args(argv)

    socket_path = os.environ.get(SOCKET_ENV_VAR, "").strip()
    if not socket_path:
        sys.stderr.write(
            f"team: {SOCKET_ENV_VAR} is not set — this binary only works inside a "
            "managed session-manager session.\n"
        )
        sys.exit(2)

    payload = _request_payload(ns)
    try:
        resp = _connect_and_exchange(socket_path, payload)
    except RuntimeError as e:
        sys.stderr.write(f"{e}\n")
        sys.exit(3)

    stdout = resp.get("stdout", "")
    stderr = resp.get("stderr", "")
    exit_code = int(resp.get("exit_code", 1))
    if stdout:
        sys.stdout.write(stdout)
        if not stdout.endswith("\n"):
            sys.stdout.write("\n")
    if stderr:
        sys.stderr.write(stderr)
        if not stderr.endswith("\n"):
            sys.stderr.write("\n")
    sys.exit(exit_code)


if __name__ == "__main__":
    main()
