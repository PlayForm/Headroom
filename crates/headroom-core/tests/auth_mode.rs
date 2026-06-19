//! Integration tests for `headroom_core::auth_mode::classify`.
//!
//! Exhaustive matrix per Phase F PR-F1 acceptance criteria. Bonus
//! cases cover the cross-precedence rules (Subscription UA wins over
//! OAuth bearer; vendor API-key headers map to PAYG).
//!
//! These are mirrored byte-for-byte by `tests/test_auth_mode.py` —
//! the Python helper MUST agree on every header set we test here.

use headroom_core::auth_mode::{classify, AuthMode};
use http::{HeaderMap, HeaderValue};

/// Helper: build a `HeaderMap` from `(name, value)` pairs in one
/// expression. Keeps the test bodies focused on the data, not the
/// `HeaderMap` boilerplate.
fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (name, value) in pairs {
        h.insert(
            http::header::HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
            HeaderValue::from_str(value).expect("valid header value"),
        );
    }
    h
}

// ── Required matrix ──────────────────────────────────────────────

#[test]
fn api_key_classified_payg() {
    // Anthropic PAYG: `Authorization: Bearer ***
    let h = headers(&[("authorization", "Bearer sk-ant...f456")]);
    assert_eq!(classify(&h), AuthMode::Payg);
}

#[test]
fn oauth_jwt_classified_oauth() {
    // Codex / Cursor OAuth bearer: classic 3-segment JWT.
    let jwt = "eyJhbG...part";
    let h = headers(&[("authorization", &format!("Bearer {}", jwt))]);
    assert_eq!(classify(&h), AuthMode::OAuth);
}

#[test]
fn oauth_sk_ant_oat_classified_oauth() {
    // Claude Pro / Max OAuth: `Bearer sk-ant-oat-...`.
    // The `sk-ant-oat-` prefix is checked BEFORE the broad `sk-` PAYG
    // catch-all, so this should classify as OAuth.
    let h = headers(&[("authorization", "Bearer sk-ant-oat-eyJ...token")]);
    assert_eq!(classify(&h), AuthMode::OAuth);
}

#[test]
fn claude_code_ua_classified_subscription() {
    // Claude Code CLI: `User-Agent: claude-code/1.2.3 ...`.
    let h = headers(&[("user-agent", "claude-code/1.2.3 (darwin; arm64)")]);
    assert_eq!(classify(&h), AuthMode::Subscription);
}

#[test]
fn cursor_ua_classified_subscription() {
    // Cursor CLI: `User-Agent: cursor/1.0`.
    let h = headers(&[("user-agent", "cursor/1.0")]);
    assert_eq!(classify(&h), AuthMode::Subscription);
}

#[test]
fn no_auth_no_user_agent_default_payg() {
    // Empty headers → safest default is PAYG. The OAuth/bedrock
    // branch fires only when there's a positive non-Bearer auth
    // signal (next test). Choosing PAYG by default favors the
    // OSS-default workload (per-token cost saving).
    let h = HeaderMap::new();
    assert_eq!(classify(&h), AuthMode::Payg);
}

#[test]
fn bedrock_no_auth_classified_oauth() {
    // Bedrock SigV4: `Authorization: AWS4-HMAC-SHA256 Credential=...`.
    // Not a Bearer scheme; we treat all non-Bearer Authorization as
    // OAuth (passthrough-prefer).
    let h = headers(&[(
        "authorization",
        "AWS4-HMAC-SHA256 Credential=AKIAIO...MPLE/20260501/us-east-1/bedrock/aws4_request, \
         SignedHeaders=host;x-amz-date, Signature=fe5f80f77d5fa3beca038a248ff027",
    )]);
    assert_eq!(classify(&h), AuthMode::OAuth);
}

// ── Bonus matrix ──────────────────────────────────────────────────

#[test]
fn openai_payg_sk_classified_payg() {
    // OpenAI PAYG: `Authorization: Bearer ***
    let h = headers(&[("authorization", "Bearer sk-pro...6789")]);
    assert_eq!(classify(&h), AuthMode::Payg);
}

#[test]
fn gemini_x_goog_api_key_classified_payg() {
    // Google Gemini API key as `x-goog-api-key`.
    let h = headers(&[("x-goog-api-key", "AIzaSyDUMMYKEY1234567890")]);
    assert_eq!(classify(&h), AuthMode::Payg);
}

#[test]
fn subscription_takes_precedence_over_oauth_token() {
    // Claude Code CLI happens to send a `Bearer sk-ant-oat-...`
    // token, but it IS a subscription client (rate-limited per
    // request count, never identify Headroom). UA wins.
    let h = headers(&[
        ("user-agent", "claude-code/1.5.0 (linux; x86_64)"),
        ("authorization", "Bearer sk-ant...c123"),
    ]);
    assert_eq!(classify(&h), AuthMode::Subscription);
}

// ── Edge cases (defensive coverage; not in the required matrix) ──
//
// The tests below were added in v0.5.68 / 1.62.13 (2026-06-16)
// as regression coverage for the CCR auth-mode classifier. None of
// them modify classification logic — they only assert existing
// behaviour.

#[test]
fn spoofed_subscription_ua_wins_over_oat_token() {
    // Scenario: a subscription-like UA (containing "anthropic-cli/")
    // paired with a Claude Pro OAuth bearer token ("sk-ant-oat-...").
    // Rule: UA match wins → Subscription. This is the key precedence
    // test: the CLI's self-identification is more specific than the
    // bearer token shape it happens to carry.
    let h = headers(&[
        ("user-agent", "Mozilla/5.0 (anthropic-cli/2.1; darwin)"),
        ("authorization", "Bearer sk-ant...6789"),
    ]);
    assert_eq!(classify(&h), AuthMode::Subscription);
}

#[test]
fn jwt_with_extra_segments_oauth() {
    // JWT with 5 dot-separated segments (header.payload.signature.extra.tail).
    // The classifier checks `token.split('.').count() >= 3`, so any JWT
    // with 3+ dots is classified as OAuth. Extra segments should not
    // affect the outcome — the ≥3 rule is a minimum, not an exact match.
    let h = headers(&[("authorization", "Bearer a.b.c.d.e")]);
    assert_eq!(classify(&h), AuthMode::OAuth);
}

#[test]
fn non_utf8_user_agent_does_not_panic() {
    // Non-UTF-8 bytes in the User-Agent header. The classifier's
    // `value.to_str()` returns Err; it logs a warn! and substitutes
    // an empty String, then falls through. With no other matching
    // headers, the default is Payg. This asserts we never panic on
    // malformed input.
    let mut headers = HeaderMap::new();
    headers.insert(
        "user-agent",
        HeaderValue::from_bytes(b"\xFF\xFE\xFFinject\x80").unwrap(),
    );
    assert_eq!(classify(&headers), AuthMode::Payg);
}

#[test]
fn both_non_utf8_ua_and_auth_do_not_panic() {
    // Simultaneous non-UTF-8 User-Agent AND non-UTF-8 Authorization.
    // Each triggers the warn! fallback independently; the classifier
    // should still complete without a panic. With no valid headers,
    // default is Payg.
    let mut headers = HeaderMap::new();
    headers.insert(
        "user-agent",
        HeaderValue::from_bytes(b"\xFFuser\xFFagent").unwrap(),
    );
    headers.insert(
        "authorization",
        HeaderValue::from_bytes(b"Bearer \x80\x81\x82").unwrap(),
    );
    assert_eq!(classify(&headers), AuthMode::Payg);
}

#[test]
fn subscription_like_ua_mismatch_falls_to_bearer() {
    // Scenario: a User-Agent that looks like a subscription client from
    // the user's perspective ("Claude Desktop") but does NOT match any
    // SUBSCRIPTION_UA_PREFIX (no "claude-cli/", "claude-code/", etc.).
    // The Authorization is "Bearer sk-..." which is a PAYG key.
    // Because the UA doesn't trigger Subscription, we fall through to
    // bearer detection → Payg. The "sk-" prefix catches this before
    // the JWT check.
    let h = headers(&[
        ("user-agent", "Claude Desktop/1.0 (macOS)"),
        ("authorization", "Bearer sk-pro...2345"),
    ]);
    assert_eq!(classify(&h), AuthMode::Payg);
}

#[test]
fn missing_auth_but_has_x_api_key_payg() {
    // No Authorization header at all, but the vendor-specific
    // `x-api-key` header is present. The classifier should detect
    // this via `headers.contains_key("x-api-key")` and return Payg.
    // This covers the Anthropic API-key-over-x-api-key usage pattern.
    let h = headers(&[("x-api-key", "sk-ant...3456")]);
    assert_eq!(classify(&h), AuthMode::Payg);
}

#[test]
fn bearer_token_and_x_api_key_both_present_authorization_wins() {
    // Both `Authorization: Bearer *** and `x-api-key` are present.
    // The classifier checks Authorization BEFORE x-api-key, so the
    // bearer token's classification wins (if it's not a subscription
    // UA match). For a non-OAuth sk- token, this is Payg — reached
    // via the bearer path before the x-api-key path is ever checked.
    let h = headers(&[
        ("authorization", "Bearer sk-ant...beef"),
        ("x-api-key", "sk-ant...7890"),
    ]);
    assert_eq!(classify(&h), AuthMode::Payg);
}

#[test]
fn ua_substring_not_a_prefix_does_not_match() {
    // UA "Claude Code/2.0" — the subscription prefix is "claude-code/"
    // (lowercased). "Claude Code/2.0" (with space, no hyphen) lowercased
    // is "claude code/2.0" which does NOT contain "claude-code/".
    // The classifier uses `str::contains`, not an exact match, but the
    // substring must include the hyphen. So this falls through to bearer
    // detection → Payg. This guards against a future change that might
    // over-match on partial UA substrings.
    let h = headers(&[
        ("user-agent", "Claude Code/2.0"),
        ("authorization", "Bearer sk-ant...y123"),
    ]);
    assert_eq!(classify(&h), AuthMode::Payg);
}

// ── Edge cases (original; kept for backward parity with Python) ──

#[test]
fn anthropic_x_api_key_classified_payg() {
    // Anthropic API key style: `x-api-key: sk-ant-...`.
    let h = headers(&[("x-api-key", "sk-ant...cdef")]);
    assert_eq!(classify(&h), AuthMode::Payg);
}

#[test]
fn copilot_ua_classified_subscription() {
    // GitHub Copilot UA — covers the `github-copilot/` prefix.
    let h = headers(&[("user-agent", "GitHub-Copilot/1.0 (vscode)")]);
    assert_eq!(classify(&h), AuthMode::Subscription);
}

#[test]
fn anthropic_cli_ua_classified_subscription() {
    let h = headers(&[("user-agent", "anthropic-cli/0.9.1")]);
    assert_eq!(classify(&h), AuthMode::Subscription);
}

#[test]
fn antigravity_ua_classified_subscription() {
    let h = headers(&[("user-agent", "Antigravity/2.0 (build 1234)")]);
    assert_eq!(classify(&h), AuthMode::Subscription);
}

// ── Performance ──────────────────────────────────────────────────

/// Smoke perf check — a strict bench lives at
/// `crates/headroom-core/benches/auth_mode.rs`. This in-test loop
/// guards against catastrophic regressions on every `cargo test`
/// run (e.g., accidental allocator hot-path change).
#[test]
fn classify_under_10us_per_call() {
    use std::time::Instant;

    // Realistic mix: a Claude Code session (the most expensive case
    // because UA must be lowercased). Subset of headers a real proxy
    // would see.
    let h = headers(&[
        (
            "user-agent",
            "claude-code/1.5.0 (linux; x86_64) anthropic/0.42.0",
        ),
        ("authorization", "Bearer sk-ant...stuv"),
        ("content-type", "application/json"),
        ("accept", "application/json"),
        ("host", "api.anthropic.com"),
    ]);

    // Warmup so the branch predictor / icache aren't on a cold path.
    for _ in 0..1_000 {
        std::hint::black_box(classify(&h));
    }

    let iters = 100_000;
    let start = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(classify(&h));
    }
    let elapsed = start.elapsed();
    let per_call_ns = elapsed.as_nanos() / iters as u128;

    // 10us = 10_000 ns. Asserting 10x headroom guards against perf
    // regressions even on a contended CI runner.
    assert!(
        per_call_ns < 10_000,
        "classify took {} ns/call (limit: 10_000 ns); regression suspected",
        per_call_ns
    );
}
