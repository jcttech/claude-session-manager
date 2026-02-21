"""Manage ClaudeSDKClient instances for multi-message sessions."""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING

from claude_agent_sdk import ClaudeAgentOptions, ClaudeSDKClient

if TYPE_CHECKING:
    pass

logger = logging.getLogger(__name__)


class SessionManager:
    """Track active ClaudeSDKClient instances keyed by session_id."""

    def __init__(self) -> None:
        self._sessions: dict[str, ClaudeSDKClient] = {}

    async def create_session(
        self,
        prompt: str,
        *,
        permission_mode: str = "bypassPermissions",
        env: dict[str, str] | None = None,
        system_prompt_append: str = "",
        max_turns: int | None = None,
    ) -> ClaudeSDKClient:
        """Create a new ClaudeSDKClient and start an execution.

        Returns the client so the caller can iterate over messages.
        The session_id is captured from the init event and stored.
        """
        options = ClaudeAgentOptions(
            system_prompt=system_prompt_append or None,
            setting_sources=["project"],
            permission_mode=permission_mode or "bypassPermissions",
            env=env or {},
            include_partial_messages=True,
            max_turns=max_turns,
        )

        client = ClaudeSDKClient(options)
        await client.connect(prompt)
        return client

    def register(self, session_id: str, client: ClaudeSDKClient) -> None:
        """Register a client under its session_id for later SendMessage calls."""
        self._sessions[session_id] = client
        logger.info("Registered session %s", session_id)

    def get(self, session_id: str) -> ClaudeSDKClient | None:
        """Get an existing client by session_id."""
        return self._sessions.get(session_id)

    async def remove(self, session_id: str) -> None:
        """Remove and disconnect a session."""
        client = self._sessions.pop(session_id, None)
        if client is not None:
            try:
                await client.disconnect()
            except Exception:
                logger.warning("Error disconnecting session %s", session_id, exc_info=True)
            logger.info("Removed session %s", session_id)

    async def interrupt(self, session_id: str) -> bool:
        """Interrupt a running session. Returns True if found and interrupted."""
        client = self._sessions.get(session_id)
        if client is None:
            return False
        try:
            await client.interrupt()
            return True
        except Exception:
            logger.warning("Error interrupting session %s", session_id, exc_info=True)
            return False

    async def shutdown(self) -> None:
        """Disconnect all sessions. Called during graceful shutdown."""
        for session_id in list(self._sessions.keys()):
            await self.remove(session_id)
