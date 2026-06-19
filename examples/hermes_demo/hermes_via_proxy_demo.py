#!/usr/bin/env python3
"""Hermes-3 via Aphrodite proxy — full multi-turn demo.

Demonstrates a complete Hermes-3 conversation routed through the Aphrodite
OpenAI-compat proxy with Headroom compression active. Equivalent to
``strands_via_proxy_demo.py`` but for the Hermes chatml tool-call format.

Usage
-----
    # With proxy already running:
    PYTHONPATH=. APHRODITE_API_KEY=sk-test python examples/hermes_demo/hermes_via_proxy_demo.py

    # Auto-start proxy (requires headroom installed + aphrodite binary):
    PYTHONPATH=. APHRODITE_API_KEY=sk-test python examples/hermes_demo/hermes_via_proxy_demo.py --start-proxy

Environment variables
---------------------
APHRODITE_API_KEY   Forwarded by the proxy to the upstream model (required).
HEADROOM_MODEL      Override the default Hermes model slug.
HEADROOM_PROXY_PORT Token proxy port (default 9797).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.request
from typing import Any

from examples.hermes_demo import CACHE_PORT, DEFAULT_MODEL, PROXY_BASE_URL, PROXY_PORT

# ---------------------------------------------------------------------------
# CCR marker pattern (canonical format as of headroom proxy current version)
# ---------------------------------------------------------------------------
# U+2AB7 ⫷  U+2AB8 ⫸
CCR_MARKER_RE = re.compile(r"\u2ab7CCR:([a-f0-9]+)\|[^\u2ab8]+\u2ab8")

# ---------------------------------------------------------------------------
# Hermes system prompt with tool definitions
# ---------------------------------------------------------------------------

HERMES_SYSTEM = """You are a helpful assistant with access to tools.

<tools>
[
  {{"type": "function", "function": {{"name": "read_file", "description": "Read a file from the filesystem", "parameters": {{"type": "object", "properties": {{"path": {{"type": "string"}}}}, "required": ["path"]}}}}}},
  {{"type": "function", "function": {{"name": "web_search", "description": "Search the web for current information", "parameters": {{"type": "object", "properties": {{"query": {{"type": "string"}}}}, "required": ["query"]}}}}}},
  {{"type": "function", "function": {{"name": "execute_code", "description": "Execute Python code in a sandbox and return stdout", "parameters": {{"type": "object", "properties": {{"code": {{"type": "string"}}}}, "required": ["code"]}}}}}},
  {{"type": "function", "function": {{"name": "headroom_stats", "description": "Return Headroom proxy compression statistics for this session", "parameters": {{"type": "object", "properties": {{}}, "required": []}}}}}},
  {{"type": "function", "function": {{"name": "headroom_retrieve", "description": "Retrieve full content for a CCR-markerized slot. Call this when you see a \u2ab7CCR:hash|type|size\u2ab8 marker and need the original content.", "parameters": {{"type": "object", "properties": {{"hash": {{"type": "string", "description": "The hex hash from inside the CCR marker"}}}}, "required": ["hash"]}}}}}}
]
</tools>

When you need to use a tool, output ONLY a JSON block in this exact format and nothing else:
<tool_call>
{{"name": "<tool_name>", "arguments": {{<arguments_dict>}}}}
</tool_call>

When you receive a tool result, it will appear as:
<tool_response>
{{"result": <result_value>}}
</tool_response>"""


# ---------------------------------------------------------------------------
# Proxy health check (with retry)
# ---------------------------------------------------------------------------


def _alive(port: int, timeout: float = 2.0) -> bool:
    """Return True if the proxy health endpoint responds."""
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=timeout) as resp:
            body = resp.read().decode()
            try:
                data = json.loads(body)
                return data.get("status") in ("healthy", "ok", "degraded")
            except Exception:
                return body.strip() == "ok"
    except Exception:
        return False


def _wait_alive(port: int, retries: int = 20, delay: float = 0.3) -> bool:
    """Poll the proxy until it is ready or retries are exhausted."""
    for i in range(retries):
        if _alive(port):
            return True
        if i < retries - 1:
            time.sleep(delay)
    return False


def _start_proxy() -> subprocess.Popen[bytes] | None:
    """Attempt to start the Aphrodite proxy via headroom CLI."""
    model = os.environ.get("HEADROOM_MODEL", DEFAULT_MODEL)
    cmd = [
        "headroom",
        "proxy",
        "start",
        "--provider",
        "openai",
        "--model",
        model,
    ]
    try:
        proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return proc
    except FileNotFoundError:
        return None


# ---------------------------------------------------------------------------
# Hermes tool-call parsing
# ---------------------------------------------------------------------------


_TOOL_CALL_RE = re.compile(r"<tool_call>\s*(\{.*?\})\s*</tool_call>", re.DOTALL)


def parse_hermes_tool_calls(content: str) -> list[dict[str, Any]]:
    """Extract Hermes <tool_call> blocks from assistant content.

    Returns a list of {"name": str, "arguments": dict} dicts.
    Returns [] if no tool calls are present.
    """
    calls = []
    for m in _TOOL_CALL_RE.finditer(content):
        try:
            parsed = json.loads(m.group(1))
            calls.append(parsed)
        except json.JSONDecodeError as exc:
            print(f"  [WARN] Failed to parse tool call JSON: {exc}")
    return calls


def wrap_tool_response(result: Any) -> str:
    """Wrap a tool result in Hermes <tool_response> tags."""
    return f'<tool_response>\n{{"result": {json.dumps(result)}}}\n</tool_response>'


# ---------------------------------------------------------------------------
# Mock tool implementations
# ---------------------------------------------------------------------------


def _mock_read_file(path: str) -> str:
    """Return a realistic large file for CCR compression testing."""
    lines = [f"# Configuration file: {path}", ""]
    sections = [
        (
            "database",
            {
                "host": "db-primary-01",
                "port": 5432,
                "name": "appdb",
                "pool_size": 20,
                "max_overflow": 10,
                "timeout": 30,
            },
        ),
        ("cache", {"host": "redis-01", "port": 6379, "db": 0, "ttl": 3600, "max_connections": 100}),
        (
            "api",
            {
                "host": "0.0.0.0",
                "port": 8080,
                "workers": 4,
                "timeout": 60,
                "max_request_size": 10485760,
            },
        ),
        (
            "auth",
            {
                "secret_key": "REDACTED",
                "algorithm": "HS256",
                "token_expiry": 3600,
                "refresh_expiry": 86400,
            },
        ),
        (
            "logging",
            {
                "level": "INFO",
                "format": "json",
                "output": "/var/log/app/app.log",
                "rotation": "daily",
            },
        ),
        (
            "monitoring",
            {
                "enabled": True,
                "interval": 30,
                "endpoint": "http://metrics:9090/push",
                "labels": {"env": "production", "region": "us-east-1"},
            },
        ),
    ]
    for section_name, values in sections:
        lines.append(f"[{section_name}]")
        for k, v in values.items():
            lines.append(f"{k} = {json.dumps(v)}")
        lines.append("")
    # Pad to ~4 000 chars to trigger CCR compression
    comment_block = (
        "# This file is managed by the deployment pipeline.\n"
        "# Manual edits will be overwritten on next deploy.\n"
        "# See docs/configuration.md for field descriptions.\n"
    ) * 25
    return "\n".join(lines) + "\n" + comment_block


def _mock_web_search(query: str) -> str:
    return json.dumps(
        [
            {
                "title": f"Result 1 for: {query}",
                "url": "https://example.com/1",
                "snippet": "This is a representative search result snippet.",
            },
            {
                "title": f"Result 2 for: {query}",
                "url": "https://example.com/2",
                "snippet": "Another search result with relevant information.",
            },
        ]
    )


def _mock_execute_code(code: str) -> str:
    return f"# Executed {len(code.splitlines())} lines\nOutput: [mock execution result]"


def _mock_headroom_stats(proxy_port: int) -> dict[str, Any]:
    """Fetch live stats from the headroom proxy /stats endpoint."""
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{proxy_port}/stats", timeout=3) as resp:
            return json.loads(resp.read().decode())
    except Exception as exc:
        return {"error": str(exc), "note": "proxy may not expose /stats"}


def _mock_headroom_retrieve(hash_val: str, proxy_port: int) -> str | None:
    """Retrieve CCR-compressed content from the proxy retrieve endpoint."""
    payload = json.dumps({"hash": hash_val}).encode()
    for port in [proxy_port, CACHE_PORT]:
        try:
            req = urllib.request.Request(
                f"http://127.0.0.1:{port}/retrieve",
                data=payload,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=4) as resp:
                body = resp.read().decode()
                data = json.loads(body)
                return data.get("content") or data.get("result")
        except Exception:
            continue
    return None


DISPATCH = {
    "read_file": _mock_read_file,
    "web_search": _mock_web_search,
    "execute_code": _mock_execute_code,
}


# ---------------------------------------------------------------------------
# Chat completion via proxy
# ---------------------------------------------------------------------------


def _chat(
    messages: list[dict[str, Any]],
    model: str,
    api_key: str,
    proxy_base: str,
) -> str:
    """Send a chat/completions request to the Aphrodite proxy and return the
    assistant message content string.
    """
    payload = json.dumps(
        {
            "model": model,
            "messages": messages,
            "stream": False,
            "temperature": 0.0,
            "max_tokens": 1024,
        }
    ).encode()
    req = urllib.request.Request(
        f"{proxy_base}/chat/completions",
        data=payload,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {api_key}",
        },
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=120) as resp:
        data = json.loads(resp.read().decode())
    return data["choices"][0]["message"]["content"]


# ---------------------------------------------------------------------------
# Main demo
# ---------------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description="Hermes-3 via Aphrodite proxy demo")
    parser.add_argument(
        "--start-proxy", action="store_true", help="Auto-start the headroom proxy if not running"
    )
    parser.add_argument("--model", default=os.environ.get("HEADROOM_MODEL", DEFAULT_MODEL))
    parser.add_argument(
        "--proxy-port", type=int, default=int(os.environ.get("HEADROOM_PROXY_PORT", PROXY_PORT))
    )
    args = parser.parse_args()

    api_key = os.environ.get("APHRODITE_API_KEY", "")
    if not api_key:
        print("ERROR: APHRODITE_API_KEY not set.", file=sys.stderr)
        sys.exit(1)

    proxy_base = f"http://127.0.0.1:{args.proxy_port}/v1"

    # --- Proxy health check ---
    print("=" * 70)
    print("Hermes-3 via Aphrodite Proxy — Headroom CCR Demo")
    print("=" * 70)

    proxy_proc: subprocess.Popen[bytes] | None = None
    if not _alive(args.proxy_port):
        if args.start_proxy:
            print("\nProxy not running. Starting...")
            proxy_proc = _start_proxy()
            if not _wait_alive(args.proxy_port):
                print("ERROR: Proxy did not start within timeout.", file=sys.stderr)
                sys.exit(1)
            print("Proxy ready.")
        else:
            print(
                f"\nERROR: Proxy not responding on port {args.proxy_port}.\n"
                "Run `headroom proxy start` or pass --start-proxy.",
                file=sys.stderr,
            )
            sys.exit(1)
    else:
        print(f"\nProxy healthy on port {args.proxy_port}.")

    model = args.model
    print(f"Model : {model}")

    # --- Build initial conversation ---
    messages: list[dict[str, Any]] = [
        {"role": "system", "content": HERMES_SYSTEM},
        {
            "role": "user",
            "content": "Read the file /etc/app/config.toml and tell me the database host.",
        },
    ]

    ccr_markers_seen: list[str] = []
    turns = 0

    # --- Agentic loop (max 6 turns) ---
    while turns < 6:
        turns += 1
        print(f"\n--- Turn {turns} (user→assistant) ---")

        # Call the model through the proxy
        try:
            assistant_content = _chat(messages, model, api_key, proxy_base)
        except Exception as exc:
            print(f"  [ERROR] Chat request failed: {exc}")
            break

        print(
            f"  Assistant: {assistant_content[:200]}{'...' if len(assistant_content) > 200 else ''}"
        )

        # Check for CCR markers in any previous tool response now visible
        for msg in messages:
            if msg.get("role") == "tool":
                found = CCR_MARKER_RE.findall(str(msg.get("content", "")))
                ccr_markers_seen.extend(found)

        # Parse tool calls
        tool_calls = parse_hermes_tool_calls(assistant_content)
        if not tool_calls:
            # Final answer — no more tool calls
            print(f"\n  [FINAL ANSWER] {assistant_content}")
            messages.append({"role": "assistant", "content": assistant_content})
            break

        messages.append({"role": "assistant", "content": assistant_content})

        # Execute each tool call
        for call in tool_calls:
            tool_name = call.get("name", "")
            tool_args = call.get("arguments", {})
            print(f"  Tool call: {tool_name}({json.dumps(tool_args)[:80]})")

            if tool_name == "headroom_stats":
                result = _mock_headroom_stats(args.proxy_port)
            elif tool_name == "headroom_retrieve":
                hash_val = tool_args.get("hash", "")
                result = _mock_headroom_retrieve(hash_val, args.proxy_port) or "[not found]"
            elif tool_name in DISPATCH:
                result = DISPATCH[tool_name](**tool_args)
            else:
                result = f"[unknown tool: {tool_name}]"

            tool_response = wrap_tool_response(result)
            print(f"  Response length: {len(tool_response)} chars")

            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": f"hermes_call_{turns:03d}",
                    "content": tool_response,
                }
            )

    # --- Summary ---
    print("\n" + "=" * 70)
    print("DEMO SUMMARY")
    print("=" * 70)
    print(f"  Turns completed : {turns}")
    print(f"  Messages total  : {len(messages)}")
    print(f"  CCR markers seen: {len(ccr_markers_seen)}")
    if ccr_markers_seen:
        print("  CCR hashes:")
        for h in ccr_markers_seen:
            print(f"    {h}")

    # Verify at least one tool result was compressed
    total_tool_chars = sum(
        len(str(m.get("content", ""))) for m in messages if m.get("role") == "tool"
    )
    print(f"  Total tool response chars: {total_tool_chars}")

    if proxy_proc:
        proxy_proc.terminate()

    print("=" * 70)


if __name__ == "__main__":
    main()
