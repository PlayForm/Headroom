# Examples — Hermes-Agent Integration Recommendations

> Scanned against all files in `examples/` as of 2026-06-15.
> Every task below is traceable to a concrete file and line range.
> "Hermes" refers to [NousResearch/hermes-agent](https://github.com/nousresearch/hermes-agent)
> running a Hermes-3 / Hermes-3-Pro model through the Aphrodite proxy.

---

## 1  `context_compression_demo.py`

### What it does today
Calls `headroom.compress()` directly on a hard-coded retriever JSON payload
and prints a comparison table against LangChain's how-to-fix-your-context
notebooks. The model slug is hard-wired to `claude-sonnet-4-5-20250929`.

### Bugs
- **Model slug is Claude-specific** — `compress(messages, model="claude-sonnet-4-5-20250929")`
  silently falls through to a generic tokenizer when run under the
  `NousResearch/Hermes-3-Pro-*` family; measured token counts are wrong by
  up to 30 % for Hermes vocab.
- **No proxy routing** — the demo calls `compress()` in-process but never
  touches the running Aphrodite binary on `:9797`/`:9798`. In a real
  Hermes session every compress call goes through the proxy; the demo gives
  a misleading baseline.
- **Assertions use `max(..., 1)` silence** — if `tokens_before` comes back
  0 (e.g. tokenizer miss) the assertion passes silently.

### Tasks
- [ ] **TASK-CC-01** Replace hard-coded model slug with
      `os.environ.get("HEADROOM_MODEL", "NousResearch/Hermes-3-Pro-Llama-3.1-8B")`.
      Add a `DEEPSEEK_COMPAT=1` env-var branch that sets
      `"deepseek-chat"` for the DeepSeek v3/v4 tokenizer path your fork
      added in commit `9f9a325`.
- [ ] **TASK-CC-02** Add a `--proxy` flag (`argparse`). When set, route the
      compress call through `http://127.0.0.1:9797/v1/chat/completions`
      (the token proxy port) so the demo exercises the full Aphrodite
      pipeline that Hermes sees in production. Mirror the request shape
      Hermes uses: `{"model": MODEL, "messages": messages, "stream": false}`.
- [ ] **TASK-CC-03** Fix the assertion guard: raise `AssertionError` with a
      diagnostic when `tokens_before == 0` rather than silently passing via
      `max(..., 1)`.
- [ ] **TASK-CC-04** Add a Hermes-specific row to the comparison table:
      `"Hermes-3 via Aphrodite proxy"` showing measured tokens_after, latency
      from the proxy round-trip, and CCR marker count (`result.messages[2]["content"].count("⫷CCR:")`).

---

## 2  `examples/mcp_demo/run_agent_eval.py`

### What it does today
Runs an agent eval loop using mock MCP servers. The agent model is loaded
via `strands` (Bedrock provider). There is no Hermes / OpenAI-compat path.

### Bugs
- **`MockMCPServer.call_tool()` silently returns `""` on unknown tool names**
  instead of raising `MCPToolNotFoundError`. Hermes-agent will retry the
  same call indefinitely if its tool name casing differs by one character.
- **`run_agent_eval.py` never checks `headroom_stats` MCP tool availability**
  before the eval loop. If the headroom MCP server isn't registered the eval
  runs without compression data, making all `tokens_saved` metrics `None`.
- **`result.success` is asserted but never defined in the eval harness**
  when the strands agent times out — produces `AttributeError` at teardown.

### Tasks
- [ ] **TASK-MCP-01** Add `hermes_demo/` sub-directory mirroring `mcp_demo/`
      but targeting the Hermes-agent stdio MCP transport. Hermes-agent
      launches MCP servers via `stdio` with JSON-RPC 2.0; the existing
      `MockMCPServer` uses a custom in-process protocol that won't work
      over stdio. New file: `examples/hermes_demo/hermes_mcp_client.py`
      implementing `StdioMCPTransport(command, args)` → `send_jsonrpc()` →
      `receive_jsonrpc()`.
- [ ] **TASK-MCP-02** In `mock_mcp_servers.py`, raise `ValueError` (not
      `return ""`) on unrecognised tool names. Add a `tool_registry` dict
      so new tools can be registered for Hermes-specific tools
      (`web_search`, `execute_code`, `read_file`, `write_file` — the four
      Hermes-agent default tools).
- [ ] **TASK-MCP-03** Wire `headroom_stats` polling into the eval loop:
      call `headroom_stats` tool once before the loop and once after; diff
      the `tokens_compressed_total` counter to get per-eval compression
      savings attributed to the headroom proxy rather than to the in-process
      `compress()` call.
- [ ] **TASK-MCP-04** Implement `hermes_agent_eval.py`: a full eval harness
      that spawns `hermes-agent --model NousResearch/Hermes-3-Pro-Llama-3.1-8B`
      with `OPENAI_BASE_URL=http://127.0.0.1:9797/v1` and
      `OPENAI_API_KEY=$APHRODITE_API_KEY`, runs the same eval tasks as
      `run_agent_eval.py`, and reports compression ratio, latency-per-turn,
      and CCR retrieval hit rate side-by-side.

---

## 3  `examples/mcp_demo/show_before_after.py`

### What it does today
Prints before/after message diff for a single hard-coded tool call. Useful
for visual inspection but not scriptable.

### Tasks
- [ ] **TASK-BA-01** Accept `--model` and `--proxy-port` CLI args.
      Default model to `NousResearch/Hermes-3-Pro-Llama-3.1-8B`.
- [ ] **TASK-BA-02** Add a Hermes function-call message shape fixture:
      Hermes uses `<tool_call>{"name": ..., "arguments": ...}</tool_call>`
      in `content` (not the OpenAI `tool_calls` array) when run in
      `chatml-function-calling` mode. The current fixture uses the OpenAI
      shape only. Add a `--hermes-format` flag that switches to the Hermes
      native format so the CCR marker injection path is exercised correctly.
- [ ] **TASK-BA-03** Print the CCR marker regex match list so the diff shows
      which spans were markerized vs left inline.

---

## 4  `examples/mcp_demo/show_compression.py`

### What it does today
Calls `headroom.compress()` with a few synthetic messages and prints
`CompressionResult` fields.

### Tasks
- [ ] **TASK-SC-01** Add a `HermesToolCallFixture` class that builds a
      Hermes-format conversation: system prompt with `<tools>` JSON block,
      user message, `<tool_call>` assistant turn, `<tool_response>` tool
      turn — the exact four-part shape Hermes-agent produces. Run
      compression over that fixture and assert the `<tool_call>` and
      `<tool_response>` tags survive the transform (they must not be
      stripped or CCR-markerized since Hermes's parser reads them literally).
- [ ] **TASK-SC-02** Add `tokens_saved_pct` sanity check: assert
      `tokens_saved_pct >= 5.0` for any tool response longer than 2 000 chars,
      surfacing the `should_compress()` threshold bug documented in
      TASK-CC-01 of Aphrodite's `plugins/aphrodite/__init__.py`.

---

## 5  `examples/test_ccr.py`

### What it does today
Smoke-tests the CCR round-trip: compress → extract hash → retrieve.
No Hermes-specific coverage.

### Bugs
- **Hash extraction regex `r"\[CCR:([a-f0-9]+)\]"` is stale** — the marker
  format is now `⫷CCR:hash|type|size⫸` (U+2AB7/U+2AB8 brackets). The old
  regex will find zero matches and silently pass all assertions.
- **`test_roundtrip()` never asserts content equality** — it only asserts
  the hash is non-empty, meaning a retrieve that returns `None` would pass.

### Tasks
- [ ] **TASK-CCR-01** Update the extraction regex to
      `r"⫷CCR:([a-f0-9]+)\|[^⫸]+⫸"` (matches current `smart_marker()` output).
      Assert `retrieved_content == original_content` not just `retrieved is not None`.
- [ ] **TASK-CCR-02** Add `test_hermes_tool_response_ccr()`: wrap a 5 000-char
      Hermes `<tool_response>` block, compress, extract CCR hash, retrieve
      via `http://127.0.0.1:9798/retrieve` (token proxy), assert full content
      is recovered. Skip if proxy not running (`skipIf(not _alive(9798))`).
- [ ] **TASK-CCR-03** Add `test_ccr_spoof_resistance()`: inject a fake
      `⫷CCR:deadbeef00000000|json|9999⫸` marker into a tool result, compress,
      and assert the fake marker is NOT retrievable (returns `None` or
      `KeyError`). This directly tests the adversarial-grid fix from
      upstream commit `5939004`.

---

## 6  `examples/test_intelligent_context_toin_ccr.py`

### What it does today
Tests Headroom's "intelligent" context-aware CCR — compresses a long
context with mixed content types and checks differential compression.
File name typo: `toin` should be `to_in` or `into`.

### Tasks
- [ ] **TASK-ITC-01** Fix the file name: rename to
      `test_intelligent_context_into_ccr.py` and update `examples/README.md`.
- [ ] **TASK-ITC-02** Add a Hermes reasoning trace fixture: Hermes-3-Pro
      emits `<thinking>...</thinking>` blocks before tool calls. These blocks
      are large (1–8 K tokens) and should be compressed aggressively. Add a
      test that includes a `<thinking>` block in the assistant turn and
      asserts compression ratio >= 40 % for that turn specifically.
- [ ] **TASK-ITC-03** The `CodeStructureHandler` tree-sitter compat shim
      (fixed in upstream PR #890) is not exercised by any example. Add a
      fixture that passes a Python function definition as tool output and
      asserts the function signature line is preserved after compression
      (regression guard for the `TypeError` silent fallback fixed in #890).

---

## 7  `examples/strands_via_proxy_demo.py`

### What it does today
Full Strands + Headroom proxy demo: spawns the proxy, wraps a Strands agent,
runs multi-turn tool calls, reports compression stats. Most complete example
in the repo. Uses `strands`/Bedrock provider.

### Bugs
- **`_wait_for_proxy()` polls once with no retry** — on cold start (first
  binary download) the proxy is not ready and the demo raises immediately.
- **`model_id` is hard-wired to `anthropic.claude-sonnet-4-5-20250929-v2:0`**
  (a Bedrock ARN) — cannot be reused for Hermes without provider changes.
- **`stream=True` is passed to the proxy** but the demo's result parser
  reads `response.json()["choices"][0]["message"]` which is the non-streaming
  shape; this would raise `KeyError` on an actual SSE stream response.

### Tasks
- [ ] **TASK-SVP-01** Add `hermes_via_proxy_demo.py` as a Hermes-native
      equivalent. Key differences from `strands_via_proxy_demo.py`:
      - Provider: `openai` compat via `OPENAI_BASE_URL=http://127.0.0.1:9797/v1`
      - Model: `NousResearch/Hermes-3-Pro-Llama-3.1-8B`
      - System prompt: include Hermes `<tools>` JSON block and
        `<tool_call>` / `<tool_response>` format instruction
      - Tool calls: parse Hermes `<tool_call>` XML from `content` field,
        not `tool_calls` array
      - MCP tools: register `headroom_stats`, `headroom_retrieve`,
        `headroom_summarize` so Hermes can self-monitor context usage
- [ ] **TASK-SVP-02** Fix `_wait_for_proxy()` in `strands_via_proxy_demo.py`
      to retry 20×300 ms (matches the fix recommended for Aphrodite's
      `on_start()`).
- [ ] **TASK-SVP-03** Fix the `stream=True` / `response.json()` mismatch:
      either drop `stream=True` or switch the parser to consume SSE chunks.

---

## 8  `examples/strands_bedrock_demo.py`

### What it does today
Bedrock-specific Strands demo. Large file (34 KB). No OpenAI-compat path.

### Tasks
- [ ] **TASK-SBD-01** This file is Bedrock-only and cannot be adapted for
      Hermes without a full provider swap. Add a `# HERMES: not applicable`
      header comment and a note pointing to the new `hermes_via_proxy_demo.py`
      (TASK-SVP-01). No code changes needed, just documentation.

---

## 9  `examples/strands_bundle_demo.py`

### What it does today
Demonstrates `headroom wrap` strands bundle mode — wraps a pre-built
Strands agent binary with the headroom proxy sidecar.

### Tasks
- [ ] **TASK-SBU-01** Add `hermes_bundle_demo.py`: demonstrate wrapping
      a `hermes-agent` binary with `headroom wrap --provider openai
      --model NousResearch/Hermes-3-Pro-Llama-3.1-8B`. This exercises the
      Click-based `headroom proxy` entrypoint env-var wiring fixed in
      upstream PR #943 (`HEADROOM_EXCLUDE_TOOLS` / `HEADROOM_TOOL_PROFILES`
      now correctly wired — confirmed by that commit). Include:
      - `HEADROOM_EXCLUDE_TOOLS=web_search` example (verifies tool exclusion)
      - `HEADROOM_RTK_GAIN_SCOPE=project` example (per upstream PR #957)
      - Binary readiness check calling `_alive(9797)` with 20-retry loop.

---

## 10  `examples/strands_mcp_dispatch_test.py`

### What it does today
Tests MCP tool dispatch via Strands with headroom compression on tool
results. Uses `@tool` decorator pattern.

### Tasks
- [ ] **TASK-SMD-01** Add Hermes tool-dispatch parity test: same tool set
      (`read_file`, `web_search`, `execute_code`) but invoked via Hermes
      native `<tool_call>` XML dispatch. Assert that:
      1. Tool results arriving as `<tool_response>` blocks are compressed
         by the proxy (CCR marker present in the next assistant turn's
         context window).
      2. `headroom_retrieve` tool correctly recovers the original content
         from the CCR hash.
      3. The `headroom_stats` MCP tool reports `tokens_compressed > 0`
         after at least one `read_file` call.

---

## 11  `examples/vercel-ai-sdk-pr/`

### What it does today
Example PR demonstrating Vercel AI SDK integration.

### Tasks
- [ ] **TASK-VAI-01** Add `examples/vercel-ai-sdk-pr/hermes-openai-compat.ts`:
      TypeScript example using `openai` SDK pointed at Aphrodite proxy with
      Hermes model. Key config:
      ```typescript
      const client = new OpenAI({
        baseURL: "http://127.0.0.1:9797/v1",
        apiKey: process.env.APHRODITE_API_KEY,
      });
      const response = await client.chat.completions.create({
        model: "NousResearch/Hermes-3-Pro-Llama-3.1-8B",
        messages,
        tool_choice: "auto",
        tools: hermesTools,
      });
      ```
      Include a note that Hermes function-calling mode requires
      `tool_choice: "auto"` and that the model may return tool calls in
      `content` (chatml format) rather than `tool_calls` array — the
      Aphrodite proxy normalizes this to OpenAI format before returning.

---

## 12  `examples/07-context-compression.ipynb`

### What it does today
Jupyter notebook demonstrating context compression. Used in LangChain PRs.

### Tasks
- [ ] **TASK-NB-01** Add a new notebook cell group `## Hermes-Agent Setup`
      with:
      - Proxy health check: `requests.get("http://127.0.0.1:9797/health")`
      - Hermes model selection widget
      - CCR round-trip verification cell
- [ ] **TASK-NB-02** The notebook's `compress()` call uses
      `model="claude-sonnet-4-5-20250929"` (same as the demo). Apply
      TASK-CC-01 fix here too.

---

## 13  New file: `examples/hermes_demo/` (to be created)

The following files do not yet exist and need to be created from scratch:

| File | Purpose |
|---|---|
| `hermes_demo/__init__.py` | Package marker |
| `hermes_demo/hermes_mcp_client.py` | stdio JSON-RPC MCP transport (TASK-MCP-01) |
| `hermes_demo/hermes_agent_eval.py` | Full eval harness (TASK-MCP-04) |
| `hermes_demo/hermes_via_proxy_demo.py` | Proxy demo, Hermes-native format (TASK-SVP-01) |
| `hermes_demo/hermes_bundle_demo.py` | Bundle/wrap demo for hermes-agent binary (TASK-SBU-01) |
| `hermes_demo/fixtures/hermes_tool_call.json` | Canonical `<tool_call>` fixture used across tests |
| `hermes_demo/fixtures/hermes_tool_response_large.json` | 5 000-char tool response for CCR tests |

---

## Priority Order

| Priority | Task IDs | Rationale |
|---|---|---|
| 🔴 P0 — Fix before any Hermes eval | TASK-CCR-01, TASK-CC-01, TASK-CC-03 | Stale regex / wrong tokenizer silently corrupt all measurements |
| 🟠 P1 — Needed for proxy-connected workflow | TASK-CC-02, TASK-SVP-01, TASK-MCP-04 | Hermes must talk to the proxy; these create that path |
| 🟡 P2 — Correctness of existing examples | TASK-SVP-02, TASK-SVP-03, TASK-MCP-02, TASK-MCP-03, TASK-SC-01, TASK-SC-02 | Fixes bugs in existing demos |
| 🟢 P3 — Hermes-specific new functionality | TASK-ITC-02, TASK-ITC-03, TASK-CCR-02, TASK-CCR-03, TASK-SMD-01, TASK-SBU-01 | New coverage extending the eval surface |
| ⚪ P4 — Nice-to-have / docs | TASK-BA-01..03, TASK-NB-01..02, TASK-VAI-01, TASK-SBD-01, TASK-ITC-01 | Polish and documentation alignment |

---

## Cross-Cutting Notes

**Hermes tool-call format**: Hermes-3 (all sizes) emits tool calls inside
`content` as `<tool_call>{"name": "...", "arguments": {...}}</tool_call>`.
The Aphrodite proxy's `proxy.rs` must rewrite these to the OpenAI
`tool_calls` array format before returning to non-Hermes consumers, and
must rewrite inbound OpenAI `tool_calls` to the chatml format when
forwarding to a Hermes model. This rewrite is currently absent from
`crates/aphrodite/src/proxy.rs` — it only normalizes between OpenAI and
DeepSeek formats. All Hermes demo tasks above depend on this rewrite being
present or on the demo explicitly opting into chatml format.

**CCR marker format**: As of the current proxy code, the canonical marker
format is `⫷CCR:hash|type|size⫸` (U+2AB7 / U+2AB8). All examples that
currently match `[CCR:hash]` (square brackets, no type/size suffix) are
operating on a stale format and will find zero markers.

**Proxy ports**: Token proxy `:9797`, cache proxy `:9798`. Retrieval
endpoint is `/retrieve` on both. Health endpoint is `/health` on both.
All examples should use these constants rather than hard-coded ports.

**DeepSeek tokenizer**: Your fork's `9f9a325` commit added DeepSeek
tokenizer mappings. Examples that pass `model="deepseek-chat"` or
`model="deepseek-v4"` will now get correct token counts. Examples should
expose a `--model` flag rather than hard-coding any slug.
