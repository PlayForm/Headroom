#!/usr/bin/env python3
"""Hermes-agent stdio MCP transport client.

Hermes-agent launches MCP servers via stdio using JSON-RPC 2.0.
This module implements the minimal transport so hermes_demo tests
can talk to a real or mock MCP server over the same protocol.

Usage
-----
    from examples.hermes_demo.hermes_mcp_client import StdioMCPClient

    async with StdioMCPClient(["python", "-m", "my_mcp_server"]) as client:
        tools = await client.list_tools()
        result = await client.call_tool("read_file", {"path": "/tmp/test.txt"})
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import subprocess
from typing import Any

log = logging.getLogger(__name__)


class MCPTransportError(RuntimeError):
    """Raised when the JSON-RPC transport fails."""


class MCPToolNotFoundError(KeyError):
    """Raised when a requested tool is not registered by the server."""


class StdioMCPClient:
    """Async JSON-RPC 2.0 MCP client over stdio.

    Implements the subset of MCP used by hermes-agent:
      - initialize / initialized handshake
      - tools/list
      - tools/call

    Parameters
    ----------
    command : list[str]
        The server process command and arguments.
    timeout : float
        Per-request timeout in seconds. Default 30.
    env : dict[str, str] | None
        Extra environment variables forwarded to the server process.
    """

    def __init__(
        self,
        command: list[str],
        timeout: float = 30.0,
        env: dict[str, str] | None = None,
    ) -> None:
        self._command = command
        self._timeout = timeout
        self._env: dict[str, str] = {**os.environ, **(env or {})}
        self._proc: asyncio.subprocess.Process | None = None
        self._next_id = 1
        self._tools: dict[str, dict[str, Any]] = {}

    # ------------------------------------------------------------------
    # Context manager
    # ------------------------------------------------------------------

    async def __aenter__(self) -> "StdioMCPClient":
        await self._start()
        await self._initialize()
        return self

    async def __aexit__(self, *_: object) -> None:
        await self._stop()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    async def list_tools(self) -> list[dict[str, Any]]:
        """Return the list of tools the server advertises."""
        resp = await self._rpc("tools/list", {})
        tools: list[dict[str, Any]] = resp.get("tools", [])
        self._tools = {t["name"]: t for t in tools}
        return tools

    async def call_tool(self, name: str, arguments: dict[str, Any]) -> Any:
        """Call a tool by name and return its result.

        Raises MCPToolNotFoundError if the tool was not advertised.
        Raises MCPTransportError on JSON-RPC error response.
        """
        if self._tools and name not in self._tools:
            raise MCPToolNotFoundError(f"Tool {name!r} not found. Available: {sorted(self._tools)}")
        resp = await self._rpc("tools/call", {"name": name, "arguments": arguments})
        # MCP spec: result is in resp["content"][0]["text"] for text tools.
        content = resp.get("content", [])
        if content and content[0].get("type") == "text":
            return content[0]["text"]
        return resp

    # ------------------------------------------------------------------
    # Internal transport
    # ------------------------------------------------------------------

    async def _start(self) -> None:
        self._proc = await asyncio.create_subprocess_exec(
            *self._command,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=self._env,
        )
        log.debug("MCP server started: pid=%d", self._proc.pid)

    async def _stop(self) -> None:
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._proc.kill()

    async def _initialize(self) -> None:
        """Perform MCP initialize / initialized handshake."""
        await self._rpc(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "clientInfo": {"name": "hermes_demo", "version": "0.1.0"},
            },
        )
        # Send the initialized notification (no response expected).
        notif = json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n"
        assert self._proc and self._proc.stdin
        self._proc.stdin.write(notif.encode())
        await self._proc.stdin.drain()

    def _next_rpc_id(self) -> int:
        rid = self._next_id
        self._next_id += 1
        return rid

    async def _rpc(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        rid = self._next_rpc_id()
        payload = (
            json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}) + "\n"
        )
        assert self._proc and self._proc.stdin and self._proc.stdout
        self._proc.stdin.write(payload.encode())
        await self._proc.stdin.drain()

        try:
            raw = await asyncio.wait_for(self._proc.stdout.readline(), timeout=self._timeout)
        except asyncio.TimeoutError as exc:
            raise MCPTransportError(
                f"Timeout waiting for response to {method!r} (id={rid})"
            ) from exc

        msg = json.loads(raw.decode())
        if "error" in msg:
            raise MCPTransportError(f"JSON-RPC error for {method!r}: {msg['error']}")
        return msg.get("result", {})


# ---------------------------------------------------------------------------
# In-process mock server for tests (no subprocess needed)
# ---------------------------------------------------------------------------


class MockHermesToolRegistry:
    """In-process mock MCP tool registry for hermes_demo unit tests.

    Raises MCPToolNotFoundError on unknown tool names (unlike the old
    mock_mcp_servers.py which silently returned an empty string).
    """

    HERMES_DEFAULT_TOOLS = {
        "read_file",
        "web_search",
        "execute_code",
        "write_file",
    }

    def __init__(self) -> None:
        self._registry: dict[str, Any] = {}
        # Register default Hermes-agent tools with stub implementations.
        self.register("read_file", self._stub_read_file)
        self.register("web_search", self._stub_web_search)
        self.register("execute_code", self._stub_execute_code)
        self.register("write_file", self._stub_write_file)

    def register(self, name: str, handler: Any) -> None:
        """Register a tool handler callable."""
        self._registry[name] = handler

    def call(self, name: str, arguments: dict[str, Any]) -> Any:
        """Dispatch a tool call. Raises MCPToolNotFoundError on unknown tools."""
        if name not in self._registry:
            raise MCPToolNotFoundError(
                f"Tool {name!r} not registered. Available: {sorted(self._registry)}"
            )
        return self._registry[name](**arguments)

    # Default stub handlers ---------------------------------------------------

    @staticmethod
    def _stub_read_file(path: str) -> str:
        return f"[stub] Contents of {path!r}: (mock file content for testing)"

    @staticmethod
    def _stub_web_search(query: str) -> str:
        return f"[stub] Search results for {query!r}: (mock search results for testing)"

    @staticmethod
    def _stub_execute_code(code: str) -> str:
        return f"[stub] Execution output for code block ({len(code)} chars): mock output"

    @staticmethod
    def _stub_write_file(path: str, content: str) -> str:
        return f"[stub] Wrote {len(content)} bytes to {path!r}"
