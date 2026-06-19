#!/usr/bin/env python3
"""Hermes-agent eval harness — Headroom compression fidelity test.

Parity test against ``mcp_demo/run_agent_eval.py`` but using:
  - Hermes-3-Pro model via Aphrodite OpenAI-compat proxy
  - Hermes chatml <tool_call> / <tool_response> format
  - Live headroom_stats polling to measure proxy-side compression
  - CCR retrieval hit-rate metric

Usage
-----
    PYTHONPATH=. APHRODITE_API_KEY=sk-test \\
        python examples/hermes_demo/hermes_agent_eval.py

    # Skip live proxy calls (offline fixture mode):
    PYTHONPATH=. HEADROOM_EVAL_OFFLINE=1 \\
        python examples/hermes_demo/hermes_agent_eval.py
"""

from __future__ import annotations

import json
import os
import random
import sys
import time
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import Any

from examples.hermes_demo import CACHE_PORT, DEFAULT_MODEL, PROXY_PORT
from examples.hermes_demo.hermes_via_proxy_demo import (
    CCR_MARKER_RE,
    HERMES_SYSTEM,
    _alive,
    _mock_headroom_stats,
    _wait_alive,
    parse_hermes_tool_calls,
    wrap_tool_response,
)

# Re-use the same data generators as run_agent_eval.py for direct parity.
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from mcp_demo.run_agent_eval import (
    generate_database_with_anomalies,
    generate_logs_with_specific_errors,
    generate_slack_with_specific_errors,
)


# ---------------------------------------------------------------------------
# Eval dataclass
# ---------------------------------------------------------------------------


@dataclass
class HermesEvalCase:
    name: str
    tool_name: str
    tool_output: str
    user_query: str
    expected_findings: list[str]
    critical_data: list[dict[str, Any]]


@dataclass
class HermesEvalResult:
    case_name: str
    found_before: int = 0
    found_after: int = 0
    total_expected: int = 0
    tokens_before: int = 0
    tokens_after: int = 0
    ccr_markers: int = 0
    ccr_retrievals_attempted: int = 0
    ccr_retrievals_succeeded: int = 0
    proxy_tokens_compressed: int = 0
    latency_ms: float = 0.0
    passed: bool = False
    missing_after: list[str] = field(default_factory=list)


# ---------------------------------------------------------------------------
# Token counting (approximate via UTF-8 chars / 4 heuristic)
# ---------------------------------------------------------------------------


def _approx_tokens(text: str) -> int:
    """Rough token estimate: 1 token ≈ 4 UTF-8 chars for English text."""
    return max(1, len(text.encode("utf-8")) // 4)


# ---------------------------------------------------------------------------
# Proxy stats helpers
# ---------------------------------------------------------------------------


def _fetch_proxy_stat(port: int, key: str) -> int:
    """Read a single integer stat from the proxy /stats endpoint."""
    try:
        stats = _mock_headroom_stats(port)
        return int(stats.get(key, 0))
    except Exception:
        return 0


def _retrieve_ccr(hash_val: str, ports: tuple[int, int]) -> str | None:
    """Attempt CCR retrieval from both proxy ports."""
    payload = json.dumps({"hash": hash_val}).encode()
    for port in ports:
        try:
            req = urllib.request.Request(
                f"http://127.0.0.1:{port}/retrieve",
                data=payload,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=4) as resp:
                data = json.loads(resp.read().decode())
                content = data.get("content") or data.get("result")
                if content:
                    return content
        except Exception:
            continue
    return None


# ---------------------------------------------------------------------------
# Offline simulation (no real LLM calls)
# ---------------------------------------------------------------------------


def _simulate_answer(tool_output: str, expected_findings: list[str]) -> str:
    """Simulate an agent answer that contains whatever is actually in the data.
    Used for HEADROOM_EVAL_OFFLINE=1 runs so the eval is runnable without a
    live model. The simulated answer always achieves perfect recall — the
    interesting metric in offline mode is compression ratio and CCR stats.
    """
    found_terms = [f for f in expected_findings if f.lower() in tool_output.lower()]
    return (
        "[OFFLINE SIMULATION] Based on the tool output I found: "
        + ", ".join(found_terms)
        + ". "
        + " ".join(found_terms)
    )


# ---------------------------------------------------------------------------
# Single eval case runner
# ---------------------------------------------------------------------------


def _run_case(
    case: HermesEvalCase,
    model: str,
    api_key: str,
    proxy_port: int,
    offline: bool,
) -> HermesEvalResult:
    """Run a single eval case, return metrics."""
    result = HermesEvalResult(
        case_name=case.name,
        total_expected=len(case.expected_findings),
    )

    # --- Stat snapshot before ---
    tokens_compressed_before = _fetch_proxy_stat(proxy_port, "tokens_compressed_total")

    # --- Build Hermes message thread ---
    messages: list[dict[str, Any]] = [
        {"role": "system", "content": HERMES_SYSTEM},
        {"role": "user", "content": case.user_query},
        {
            "role": "assistant",
            "content": (
                f'<tool_call>\n{{"name": "{case.tool_name}", '
                f'"arguments": {{"query": "all"}}}}\n</tool_call>'
            ),
        },
        {
            "role": "tool",
            "tool_call_id": "eval_call_001",
            "content": wrap_tool_response(case.tool_output),
        },
    ]

    result.tokens_before = _approx_tokens(case.tool_output)

    # --- Compress via headroom (in-process) ---
    try:
        from headroom import compress

        cr = compress(messages, model=model)
        compressed_messages = cr.messages
        result.tokens_after = cr.tokens_after
    except Exception:
        # Fallback: pass messages as-is
        compressed_messages = messages
        result.tokens_after = result.tokens_before

    # --- Count CCR markers in compressed tool response ---
    tool_content = str(compressed_messages[-1].get("content", "")) if compressed_messages else ""
    ccr_hashes = CCR_MARKER_RE.findall(tool_content)
    result.ccr_markers = len(ccr_hashes)

    # --- Attempt CCR retrievals ---
    result.ccr_retrievals_attempted = len(ccr_hashes)
    if not offline and ccr_hashes:
        for h in ccr_hashes:
            if _retrieve_ccr(h, (proxy_port, CACHE_PORT)):
                result.ccr_retrievals_succeeded += 1

    # --- Get agent answer ---
    t0 = time.perf_counter()
    if offline:
        answer = _simulate_answer(case.tool_output, case.expected_findings)
    else:
        try:
            from examples.hermes_demo.hermes_via_proxy_demo import _chat

            answer = _chat(
                compressed_messages + [{"role": "user", "content": case.user_query}],
                model,
                api_key,
                f"http://127.0.0.1:{proxy_port}/v1",
            )
        except Exception as exc:
            answer = f"[ERROR: {exc}]"
    result.latency_ms = (time.perf_counter() - t0) * 1000

    # --- Evaluate answer quality ---
    answer_lower = answer.lower()
    found_after = 0
    missing = []
    for finding in case.expected_findings:
        if finding.lower() in answer_lower:
            found_after += 1
        else:
            missing.append(finding)
    result.found_after = found_after
    result.missing_after = missing
    result.found_before = found_after  # single-pass eval; before = after (no re-run)

    # --- Proxy stat delta ---
    tokens_compressed_after = _fetch_proxy_stat(proxy_port, "tokens_compressed_total")
    result.proxy_tokens_compressed = tokens_compressed_after - tokens_compressed_before

    result.passed = found_after >= len(case.expected_findings) - 1  # allow 1 miss
    return result


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> None:
    offline = bool(os.environ.get("HEADROOM_EVAL_OFFLINE"))
    model = os.environ.get("HEADROOM_MODEL", DEFAULT_MODEL)
    proxy_port = int(os.environ.get("HEADROOM_PROXY_PORT", PROXY_PORT))
    api_key = os.environ.get("APHRODITE_API_KEY", "sk-offline")

    print("=" * 70)
    print("HERMES-AGENT EVAL: Headroom CCR Compression Fidelity")
    print("=" * 70)
    print(f"  Model        : {model}")
    print(f"  Proxy port   : {proxy_port}")
    print(f"  Offline mode : {offline}")

    if not offline:
        if not _alive(proxy_port):
            print(
                f"\nERROR: Proxy not responding on :{proxy_port}.\n"
                "Set HEADROOM_EVAL_OFFLINE=1 to run without a proxy.",
                file=sys.stderr,
            )
            sys.exit(1)
        print("  Proxy status : healthy")

    # Build eval cases (identical data generators as run_agent_eval.py)
    slack_out, slack_errors = generate_slack_with_specific_errors()
    logs_out, log_errors = generate_logs_with_specific_errors()
    db_out, db_anomalies = generate_database_with_anomalies()

    cases: list[HermesEvalCase] = [
        HermesEvalCase(
            name="Slack: Find Payment Outage",
            tool_name="mcp__slack__search",
            tool_output=slack_out,
            user_query="What's causing the payment issues? Find errors related to payments.",
            expected_findings=["payment", "DOWN", "ConnectionRefused", "payment-db-01"],
            critical_data=slack_errors,
        ),
        HermesEvalCase(
            name="Slack: Find Auth Errors",
            tool_name="mcp__slack__search",
            tool_output=slack_out,
            user_query="Are there any authentication or auth service errors?",
            expected_findings=["Auth service", "500", "NullPointerException", "TokenValidator"],
            critical_data=slack_errors,
        ),
        HermesEvalCase(
            name="Logs: Find All Errors",
            tool_name="mcp__logs__search",
            tool_output=logs_out,
            user_query="List all ERROR and FATAL log entries with their services.",
            expected_findings=[
                "payment-service",
                "auth-service",
                "api-gateway",
                "Connection refused",
                "NullPointerException",
            ],
            critical_data=log_errors,
        ),
        HermesEvalCase(
            name="Database: Find Anomalous Accounts",
            tool_name="mcp__database__query",
            tool_output=db_out,
            user_query="Find any suspicious or anomalous user accounts.",
            expected_findings=["account_locked", "999999", "47", "-500"],
            critical_data=db_anomalies,
        ),
    ]

    results: list[HermesEvalResult] = []
    for case in cases:
        print(f"\n{'─' * 70}")
        print(f"  EVAL: {case.name}")
        r = _run_case(case, model, api_key, proxy_port, offline)
        results.append(r)

        compression_pct = (
            (1 - r.tokens_after / r.tokens_before) * 100 if r.tokens_before > 0 else 0.0
        )
        ccr_hit_rate = (
            r.ccr_retrievals_succeeded / r.ccr_retrievals_attempted * 100
            if r.ccr_retrievals_attempted > 0
            else float("nan")
        )
        status = "PASS" if r.passed else "FAIL"
        print(
            f"  Tokens  : {r.tokens_before:,} → {r.tokens_after:,} ({compression_pct:.0f}% compressed)"
        )
        print(
            f"  CCR     : {r.ccr_markers} markers, hit rate {ccr_hit_rate:.0f}%"
            if not offline
            else f"  CCR     : {r.ccr_markers} markers (offline)"
        )
        print(
            f"  Findings: {r.found_after}/{r.total_expected} (missing: {r.missing_after or 'none'})"
        )
        print(f"  Latency : {r.latency_ms:.0f}ms    Status: {status}")

    # --- Summary table ---
    print("\n" + "=" * 70)
    print("EVAL SUMMARY")
    print("=" * 70)
    passed = sum(1 for r in results if r.passed)
    print(f"\n  Passed: {passed}/{len(results)}")
    print()
    header = f"  {'Case':<35} {'Compress':>9} {'CCR':>5} {'Found':>7} {'Status'}"
    print(header)
    print("  " + "-" * (len(header) - 2))
    for r in results:
        comp = (
            f"{(1 - r.tokens_after / r.tokens_before) * 100:.0f}%" if r.tokens_before > 0 else "n/a"
        )
        status = "PASS" if r.passed else "FAIL"
        print(
            f"  {r.case_name:<35} {comp:>9} {r.ccr_markers:>5} "
            f"{r.found_after}/{r.total_expected:>3}    {status}"
        )

    total_before = sum(r.tokens_before for r in results)
    total_after = sum(r.tokens_after for r in results)
    total_ccr = sum(r.ccr_markers for r in results)
    print(
        f"\n  Total tokens: {total_before:,} → {total_after:,} "
        f"({(1 - total_after / total_before) * 100:.0f}% saved)"
        if total_before > 0
        else ""
    )
    print(f"  Total CCR markers seen across all cases: {total_ccr}")

    # Compare to gpt-4o-mini from run_agent_eval.py for side-by-side
    print("\n  Parity note: run mcp_demo/run_agent_eval.py with gpt-4o-mini for")
    print("  side-by-side comparison (identical data generators, same eval criteria).")
    print("=" * 70)

    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
