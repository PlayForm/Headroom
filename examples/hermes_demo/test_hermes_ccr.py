#!/usr/bin/env python3
"""CCR round-trip tests for Hermes tool-call format.

Covers TASK-CCR-01, TASK-CCR-02, TASK-CCR-03 from examples/recommendations.md.

Run with:
    PYTHONPATH=. pytest examples/hermes_demo/test_hermes_ccr.py -v

Or standalone:
    PYTHONPATH=. python examples/hermes_demo/test_hermes_ccr.py
"""

from __future__ import annotations

import json
import os
import re
import sys
import unittest
import urllib.request
from pathlib import Path

from examples.hermes_demo import CACHE_PORT, DEFAULT_MODEL, PROXY_PORT
from examples.hermes_demo.hermes_via_proxy_demo import (
    CCR_MARKER_RE,
    wrap_tool_response,
)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _alive(port: int, timeout: float = 1.5) -> bool:
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=timeout) as resp:
            body = resp.read().decode()
            try:
                return json.loads(body).get("status") in ("healthy", "ok", "degraded")
            except Exception:
                return body.strip() == "ok"
    except Exception:
        return False


def _retrieve(hash_val: str, port: int, timeout: float = 4.0) -> str | None:
    payload = json.dumps({"hash": hash_val}).encode()
    try:
        req = urllib.request.Request(
            f"http://127.0.0.1:{port}/retrieve",
            data=payload,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            data = json.loads(resp.read().decode())
            return data.get("content") or data.get("result")
    except Exception:
        return None


def _compress(messages: list[dict], model: str | None = None) -> object:
    from headroom import compress

    return compress(messages, model=model or DEFAULT_MODEL)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


class TestCCRMarkerFormat(unittest.TestCase):
    """TASK-CCR-01: Verify correct U+2AB7/U+2AB8 marker regex."""

    def test_old_square_bracket_regex_finds_nothing(self) -> None:
        """The stale [CCR:hash] pattern must NOT match current markers."""
        stale_re = re.compile(r"\[CCR:([a-f0-9]+)\]")
        # Build a realistic compressed message to test against
        large_content = "A" * 3000  # trigger CCR
        messages = [
            {"role": "user", "content": "Read this data."},
            {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": '{"path": "/tmp/x"}'},
                    }
                ],
            },
            {"role": "tool", "tool_call_id": "c1", "content": large_content},
        ]
        try:
            cr = _compress(messages)
            compressed = str(cr.messages[-1].get("content", ""))
            # If headroom is active, there may be CCR markers.
            old_matches = stale_re.findall(compressed)
            new_matches = CCR_MARKER_RE.findall(compressed)
            # If new_matches found markers, old_matches must find FEWER (or zero).
            self.assertLessEqual(
                len(old_matches),
                len(new_matches),
                msg=(
                    f"Old regex found {len(old_matches)} matches, new found "
                    f"{len(new_matches)}. Old regex should not outperform new."
                ),
            )
        except ImportError:
            self.skipTest("headroom not installed")

    def test_new_marker_regex_matches_canonical_format(self) -> None:
        """CCR_MARKER_RE must match the ⫷CCR:hash|type|size⫸ format."""
        canonical = "\u2ab7CCR:deadbeef01234567|json|4096\u2ab8"
        matches = CCR_MARKER_RE.findall(canonical)
        self.assertEqual(
            matches,
            ["deadbeef01234567"],
            "CCR_MARKER_RE must extract the hash from canonical markers",
        )

    def test_marker_regex_handles_long_type_field(self) -> None:
        """Type field may be multi-word (e.g. 'code/python')."""
        marker = "\u2ab7CCR:aabbccdd00112233|code/python|8192\u2ab8"
        matches = CCR_MARKER_RE.findall(marker)
        self.assertEqual(matches, ["aabbccdd00112233"])


class TestHermesToolResponseCCR(unittest.TestCase):
    """TASK-CCR-02: Hermes <tool_response> block CCR round-trip."""

    def setUp(self) -> None:
        fixture_path = Path(__file__).parent / "fixtures" / "hermes_tool_response_large.json"
        if fixture_path.exists():
            raw = json.loads(fixture_path.read_text())
            self._large_content = raw["content"]
        else:
            # Fallback: build inline
            self._large_content = wrap_tool_response(
                {"log_entries": [{"level": "INFO", "msg": f"entry {i}"} for i in range(200)]}
            )

    def test_content_length_triggers_ccr(self) -> None:
        """Large Hermes tool response must be long enough to trigger CCR."""
        self.assertGreater(
            len(self._large_content),
            2000,
            "Fixture must be > 2000 chars to trigger CCR compression.",
        )

    @unittest.skipUnless(_alive(PROXY_PORT), "proxy not running on :9797")
    def test_round_trip_via_token_proxy(self) -> None:
        """TASK-CCR-02: compress Hermes tool response, retrieve from token proxy."""
        messages = [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": "Summarize the logs."},
            {
                "role": "assistant",
                "content": ('<tool_call>\n{"name": "read_logs", "arguments": {}}\n</tool_call>'),
            },
            {
                "role": "tool",
                "tool_call_id": "hermes_ccr_test_001",
                "content": self._large_content,
            },
        ]
        cr = _compress(messages)
        compressed_content = str(cr.messages[-1].get("content", ""))
        hashes = CCR_MARKER_RE.findall(compressed_content)
        self.assertGreater(
            len(hashes),
            0,
            "Expected at least one CCR marker in compressed large tool response.",
        )
        # Attempt retrieval from token proxy
        retrieved = _retrieve(hashes[0], PROXY_PORT)
        self.assertIsNotNone(
            retrieved,
            f"CCR retrieve returned None for hash {hashes[0]!r} on port {PROXY_PORT}.",
        )
        self.assertIn(
            "FATAL",
            retrieved or "",
            "Retrieved content must contain the critical FATAL log entry.",
        )


class TestCCRSpoofResistance(unittest.TestCase):
    """TASK-CCR-03: Fake CCR markers must not be retrievable from the proxy."""

    @unittest.skipUnless(_alive(PROXY_PORT), "proxy not running on :9797")
    def test_fake_marker_not_retrievable(self) -> None:
        """A fake ⫷CCR:deadbeef...⫸ marker must return None from the proxy."""
        fake_hash = "deadbeef" * 8  # 64 hex chars, not a real stored hash
        result = _retrieve(fake_hash, PROXY_PORT)
        self.assertIsNone(
            result,
            f"Proxy must return None for fake hash {fake_hash!r}. "
            "This tests the adversarial-grid CCR spoof resistance (upstream PR #918).",
        )

    @unittest.skipUnless(_alive(CACHE_PORT), "cache proxy not running on :9798")
    def test_fake_marker_not_retrievable_cache_proxy(self) -> None:
        """Same check on cache proxy port."""
        fake_hash = "cafebabe" * 8
        result = _retrieve(fake_hash, CACHE_PORT)
        self.assertIsNone(
            result,
            f"Cache proxy must return None for fake hash {fake_hash!r}.",
        )

    def test_fake_marker_survives_compression(self) -> None:
        """A fake CCR marker injected into tool content must not be 'compressed away'.

        The proxy must not treat fake markers as already-compressed and skip them.
        After compress(), the fake marker string must still appear literally in the
        output so the agent sees it and (if it tries headroom_retrieve) gets None.
        """
        fake_marker = "\u2ab7CCR:deadbeef01234567|json|9999\u2ab8"
        tool_content = f"Normal content. {fake_marker} More content here. " + "X" * 500
        messages = [
            {"role": "user", "content": "Analyze."},
            {
                "role": "assistant",
                "content": '<tool_call>\n{"name": "read", "arguments": {}}\n</tool_call>',
            },
            {"role": "tool", "tool_call_id": "t1", "content": tool_content},
        ]
        try:
            cr = _compress(messages)
            output = str(cr.messages[-1].get("content", ""))
            # The fake marker must still be present (not silently dropped).
            self.assertIn(
                fake_marker,
                output,
                "Fake CCR marker was silently removed during compression. "
                "It should be preserved so the agent can attempt retrieval.",
            )
        except ImportError:
            self.skipTest("headroom not installed")


class TestHermesTagPreservation(unittest.TestCase):
    """TASK-SC-01: Hermes <tool_call> and <tool_response> tags must survive compression."""

    def test_tool_call_tag_not_stripped(self) -> None:
        """<tool_call> in assistant content must not be CCR-markerized or stripped."""
        assistant_msg = (
            '<tool_call>\n{"name": "read_file", "arguments": {"path": "/etc/hosts"}}\n</tool_call>'
        )
        messages = [
            {"role": "user", "content": "Read /etc/hosts."},
            {"role": "assistant", "content": assistant_msg},
            {"role": "tool", "tool_call_id": "t1", "content": "127.0.0.1 localhost"},
        ]
        try:
            cr = _compress(messages)
            out_assistant = str(cr.messages[1].get("content", ""))
            self.assertIn(
                "<tool_call>",
                out_assistant,
                "<tool_call> tag must be preserved in compressed assistant message.",
            )
            self.assertIn(
                "read_file",
                out_assistant,
                "Tool name must be preserved inside <tool_call> tag.",
            )
        except ImportError:
            self.skipTest("headroom not installed")

    def test_tool_response_tag_not_stripped(self) -> None:
        """<tool_response> wrapping in tool content must survive compression."""
        tool_content = wrap_tool_response("small result")
        messages = [
            {"role": "user", "content": "Do something."},
            {
                "role": "assistant",
                "content": '<tool_call>\n{"name": "t", "arguments": {}}\n</tool_call>',
            },
            {"role": "tool", "tool_call_id": "t1", "content": tool_content},
        ]
        try:
            cr = _compress(messages)
            out_tool = str(cr.messages[-1].get("content", ""))
            self.assertIn(
                "<tool_response>",
                out_tool,
                "<tool_response> tag must be preserved in compressed tool message.",
            )
        except ImportError:
            self.skipTest("headroom not installed")


if __name__ == "__main__":
    unittest.main(verbosity=2)
