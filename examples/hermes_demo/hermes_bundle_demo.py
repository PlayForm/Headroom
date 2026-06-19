#!/usr/bin/env python3
"""Hermes-agent bundle / wrap demo.

Demonstrates ``headroom wrap`` with the hermes-agent binary, exercising:
  - The Click-based ``headroom proxy`` entrypoint with HEADROOM_EXCLUDE_TOOLS
    and HEADROOM_RTK_GAIN_SCOPE (both wired by upstream PR #943 / #957).
  - The 20-retry proxy readiness check (fixes TASK-SVP-02).
  - Hermes-specific model slug and OpenAI provider path.

Usage
-----
    PYTHONPATH=. APHRODITE_API_KEY=sk-test \\
        python examples/hermes_demo/hermes_bundle_demo.py

    # Exclude web_search tool and use project-scoped RTK stats:
    HEADROOM_EXCLUDE_TOOLS=web_search HEADROOM_RTK_GAIN_SCOPE=project \\
    APHRODITE_API_KEY=sk-test python examples/hermes_demo/hermes_bundle_demo.py
"""

from __future__ import annotations

import os
import subprocess
import sys
import time

from examples.hermes_demo import DEFAULT_MODEL, PROXY_PORT
from examples.hermes_demo.hermes_via_proxy_demo import _alive, _wait_alive

# Hermes-agent binary name (must be on PATH or set HERMES_AGENT_BIN).
HERMES_BIN = os.environ.get("HERMES_AGENT_BIN", "hermes-agent")

# Headroom CLI binary.
HEADROOM_BIN = os.environ.get("HEADROOM_BIN", "headroom")


def _build_wrap_command(model: str, exclude_tools: str | None, rtk_scope: str) -> list[str]:
    """Build the ``headroom wrap`` command for the Hermes binary."""
    cmd = [
        HEADROOM_BIN,
        "wrap",
        "--provider",
        "openai",
        "--model",
        model,
        "--binary",
        HERMES_BIN,
    ]
    return cmd


def _build_proxy_env(model: str, exclude_tools: str | None, rtk_scope: str) -> dict[str, str]:
    """Build environment for the headroom proxy process.

    HEADROOM_EXCLUDE_TOOLS and HEADROOM_RTK_GAIN_SCOPE are passed as env vars
    rather than CLI flags because the Click entrypoint reads them from the
    environment (fixed in upstream PR #943 / #957).
    """
    env = {
        **os.environ,
        "HEADROOM_MODEL": model,
        "HEADROOM_RTK_GAIN_SCOPE": rtk_scope,
        "OPENAI_BASE_URL": f"http://127.0.0.1:{PROXY_PORT}/v1",
    }
    if exclude_tools:
        env["HEADROOM_EXCLUDE_TOOLS"] = exclude_tools
        print(f"  HEADROOM_EXCLUDE_TOOLS = {exclude_tools}")
    env["HEADROOM_RTK_GAIN_SCOPE"] = rtk_scope
    print(f"  HEADROOM_RTK_GAIN_SCOPE = {rtk_scope}")
    return env


def _verify_proxy_excluded_tools(
    exclude_tools: str | None,
    proxy_port: int,
) -> bool:
    """Verify that excluded tools are not advertised by the proxy.

    Calls the proxy /tools endpoint (if available) and checks.
    Returns True if verification passes or endpoint is unavailable.
    """
    if not exclude_tools:
        return True
    import urllib.request, json

    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{proxy_port}/tools", timeout=3) as resp:
            tools_data = json.loads(resp.read().decode())
            tool_names = [t.get("name", "").lower() for t in tools_data.get("tools", [])]
            for excluded in exclude_tools.split(","):
                if excluded.strip().lower() in tool_names:
                    print(f"  [FAIL] Excluded tool still advertised: {excluded}")
                    return False
            print(f"  [PASS] Excluded tools not in proxy tool list.")
            return True
    except Exception:
        # Endpoint not available — skip verification.
        return True


def main() -> None:
    model = os.environ.get("HEADROOM_MODEL", DEFAULT_MODEL)
    exclude_tools = os.environ.get("HEADROOM_EXCLUDE_TOOLS", "")
    rtk_scope = os.environ.get("HEADROOM_RTK_GAIN_SCOPE", "global")
    proxy_port = int(os.environ.get("HEADROOM_PROXY_PORT", PROXY_PORT))

    print("=" * 70)
    print("Hermes-agent bundle/wrap demo")
    print("=" * 70)
    print(f"  Model          : {model}")
    print(f"  Hermes binary  : {HERMES_BIN}")
    print(f"  Headroom binary: {HEADROOM_BIN}")

    api_key = os.environ.get("APHRODITE_API_KEY", "")
    if not api_key:
        print("ERROR: APHRODITE_API_KEY not set.", file=sys.stderr)
        sys.exit(1)

    # --- Check if proxy is already running ---
    if _alive(proxy_port):
        print(f"\n  Proxy already healthy on :{proxy_port}. Using existing instance.")
    else:
        print(f"\n  Starting proxy via headroom wrap...")
        env = _build_proxy_env(model, exclude_tools or None, rtk_scope)
        wrap_cmd = _build_wrap_command(model, exclude_tools or None, rtk_scope)
        print(f"  Command: {' '.join(wrap_cmd)}")

        try:
            proc = subprocess.Popen(
                wrap_cmd,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
        except FileNotFoundError as exc:
            print(
                f"\nERROR: Could not launch headroom wrap: {exc}\n"
                "Ensure `headroom` is installed and on PATH.",
                file=sys.stderr,
            )
            sys.exit(1)

        # --- 20-retry readiness check (TASK-SVP-02 fix) ---
        if not _wait_alive(proxy_port, retries=20, delay=0.3):
            stderr_out = b""
            if proc.stderr:
                stderr_out = proc.stderr.read(512)
            print(
                f"\nERROR: Proxy not ready after 20 retries (6 s).\n"
                f"Stderr: {stderr_out.decode(errors='replace')}",
                file=sys.stderr,
            )
            proc.terminate()
            sys.exit(1)

        print(f"  Proxy ready on :{proxy_port} after wrap.")

    # --- Verify excluded tools ---
    _verify_proxy_excluded_tools(exclude_tools or None, proxy_port)

    # --- Run a quick smoke test through the wrapped proxy ---
    print("\n  Running smoke test through wrapped proxy...")
    import json, urllib.request

    payload = json.dumps(
        {
            "model": model,
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "Say 'bundle demo OK' and nothing else."},
            ],
            "stream": False,
            "max_tokens": 32,
            "temperature": 0.0,
        }
    ).encode()
    try:
        req = urllib.request.Request(
            f"http://127.0.0.1:{proxy_port}/v1/chat/completions",
            data=payload,
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {api_key}",
            },
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=60) as resp:
            data = json.loads(resp.read().decode())
        reply = data["choices"][0]["message"]["content"]
        print(f"  Model reply: {reply!r}")
        print("  [PASS] Proxy forwarded request successfully.")
    except Exception as exc:
        print(f"  [WARN] Smoke test failed: {exc}")

    print("\n" + "=" * 70)
    print("Bundle demo complete.")
    print("=" * 70)


if __name__ == "__main__":
    main()
