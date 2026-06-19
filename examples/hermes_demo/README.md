# `hermes_demo/` — Hermes-3 + Headroom Integration

This directory contains examples and tests for running
[NousResearch/Hermes-3-Pro](https://github.com/nousresearch/hermes-agent)
through the [Aphrodite](https://github.com/playform/aphrodite) OpenAI-compat
proxy with Headroom context compression active.

## Files

| File | Purpose |
|------|------|
| `__init__.py` | Package constants (ports, default model slug) |
| `hermes_mcp_client.py` | Async stdio JSON-RPC 2.0 MCP transport + in-process mock registry |
| `hermes_via_proxy_demo.py` | Full multi-turn Hermes chat demo through the Aphrodite proxy |
| `hermes_agent_eval.py` | Compression fidelity eval — parity with `mcp_demo/run_agent_eval.py` |
| `hermes_bundle_demo.py` | `headroom wrap` bundle demo for the hermes-agent binary |
| `test_hermes_ccr.py` | CCR round-trip, spoof-resistance, and tag-preservation tests |
| `fixtures/hermes_tool_call.json` | Canonical `<tool_call>` / `<tool_response>` conversation fixture |
| `fixtures/hermes_tool_response_large.json` | ~5 000-char log output for CCR compression tests |

## Quick Start

```bash
# 1. Start the Aphrodite proxy (or use --start-proxy flag)
export APHRODITE_API_KEY=sk-your-key
headroom proxy start --provider openai --model NousResearch/Hermes-3-Pro-Llama-3.1-8B

# 2. Run the multi-turn demo
PYTHONPATH=. python examples/hermes_demo/hermes_via_proxy_demo.py

# 3. Run the compression fidelity eval (no live model needed)
PYTHONPATH=. HEADROOM_EVAL_OFFLINE=1 python examples/hermes_demo/hermes_agent_eval.py

# 4. Run CCR tests (proxy must be running for live tests; offline tests always run)
PYTHONPATH=. pytest examples/hermes_demo/test_hermes_ccr.py -v

# 5. Bundle demo (headroom wrap with hermes-agent binary)
HEADROOM_EXCLUDE_TOOLS=web_search HEADROOM_RTK_GAIN_SCOPE=project \
  python examples/hermes_demo/hermes_bundle_demo.py
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `APHRODITE_API_KEY` | — | API key forwarded to upstream model **(required for live runs)** |
| `HEADROOM_MODEL` | `NousResearch/Hermes-3-Pro-Llama-3.1-8B` | Model slug passed to proxy |
| `HEADROOM_PROXY_PORT` | `9797` | Aphrodite token-proxy port |
| `HEADROOM_CACHE_PORT` | `9798` | Aphrodite cache-proxy port |
| `HEADROOM_EVAL_OFFLINE` | unset | Set to `1` to run eval without a live model |
| `HEADROOM_EXCLUDE_TOOLS` | unset | Comma-separated tool names to exclude from proxy (PR #943) |
| `HEADROOM_RTK_GAIN_SCOPE` | `global` | `global` or `project` RTK stats scope (PR #957) |
| `HERMES_AGENT_BIN` | `hermes-agent` | Path to the hermes-agent binary |

## Hermes Tool-Call Format

Hermes-3 uses the **chatml function-calling** format when loaded with
`--chat-template chatml-function-calling`. Tool calls appear in `content`:

```xml
<tool_call>
{"name": "read_file", "arguments": {"path": "/etc/hosts"}}
</tool_call>
```

Tool responses appear as:

```xml
<tool_response>
{"result": "127.0.0.1 localhost"}
</tool_response>
```

This is **different from the OpenAI `tool_calls` array**. The Aphrodite proxy
normalizes between both formats, but the Hermes demos use the native format
so the CCR marker injection path is exercised correctly.

## CCR Marker Format

The canonical CCR marker format (headroom current version):

```
⫷CCR:deadbeef01234567|json|4096⫸
```

- U+2AB7 `⫷` — open bracket
- `CCR:` — prefix
- hex hash — content hash (64 hex chars)
- `|type|size` — content type and original byte size
- U+2AB8 `⫸` — close bracket

> **Note:** Any code matching the stale `[CCR:hash]` square-bracket format
> will find zero markers. Update to `\u2ab7CCR:([a-f0-9]+)\|[^\u2ab8]+\u2ab8`.

## Relation to `mcp_demo/`

`hermes_agent_eval.py` reuses the same data generators from
`mcp_demo/run_agent_eval.py` (Slack, logs, database fixtures) so you can
run both evals and compare compression ratio, latency, and CCR stats
side-by-side between `gpt-4o-mini` and `Hermes-3-Pro`.
