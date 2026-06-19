//! headroom-ffi: Full Aphrodite plugin runtime as a C ABI cdylib.
//!
//! Load this dylib from any language (Python ctypes, Node ffi, Go cgo, Rust FFI)
//! and get the complete Aphrodite compression engine — classify, compress,
//! retrieve, transform hooks, catalog, stats, config hot-reload, skills.
//!
//! ## Architecture
//!
//! ```text
//! Any Agent → aphrodite_init(config_path) → handle (opaque ptr)
//!                aphrodite_transform(handle, content, tool) → JSON
//!                aphrodite_terminal(handle, content) → JSON
//!                aphrodite_catalog(handle, mode) → JSON
//!                aphrodite_stats(handle) → JSON
//!                aphrodite_compress(handle, content, hint) → JSON
//!                aphrodite_retrieve(handle, hash) → content
//!                aphrodite_reload(handle) → reload config from disk
//!                aphrodite_destroy(handle)
//! ```
//!
//! State is per-handle — safe for multi-agent use.

mod hooks;
mod marker;
mod state;

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::{LazyLock, Mutex};

use headroom_core::transforms;
use state::AphroditeState;

// ── Global handle registry ──────────────────────────────────────────
static HANDLES: LazyLock<Mutex<HashMap<usize, AphroditeState>>> =
    LazyLock::new(|| Mutex::new(HashMap::<usize, AphroditeState>::new()));
static NEXT_ID: LazyLock<Mutex<usize>> = LazyLock::new(|| Mutex::new(1usize));

fn alloc_handle(state: AphroditeState) -> usize {
    let mut id = NEXT_ID.lock().unwrap();
    let hid = *id;
    *id += 1;
    HANDLES.lock().unwrap().insert(hid, state);
    hid
}

fn with_handle<F, R>(hid: usize, f: F) -> *mut c_char
where
    F: FnOnce(&mut AphroditeState) -> Result<String, String>,
{
    let mut handles = HANDLES.lock().unwrap();
    match handles.get_mut(&hid) {
        Some(state) => match f(state) {
            Ok(json) => CString::new(json).unwrap().into_raw(),
            Err(e) => to_json_error(&e),
        },
        None => to_json_error(&format!("invalid handle: {}", hid)),
    }
}

// ── Public C ABI ─────────────────────────────────────────────────────

/// Create a new Aphrodite runtime instance.
/// config_path: path to aphrodite.toml (or empty string for defaults).
/// Returns an opaque handle (usize as string).
#[no_mangle]
pub extern "C" fn aphrodite_init(config_path: *const c_char) -> *mut c_char {
    let path = unsafe { CStr::from_ptr(config_path) }.to_string_lossy();
    let mut state = AphroditeState::default();

    if !path.is_empty() {
        if let Ok(toml_str) = std::fs::read_to_string(path.as_ref()) {
            if let Ok(table) = toml_str.parse::<toml::Table>() {
                if let Some(compression) = table.get("compression").and_then(|v| v.as_table()) {
                    if let Some(v) = compression.get("context_engine").and_then(|v| v.as_bool()) {
                        state.context_engine_enabled = v;
                    }
                    if let Some(v) = compression.get("engine_threshold_pct").and_then(|v| v.as_integer()) {
                        state.engine_threshold_pct = v as u64;
                    }
                    if let Some(v) = compression.get("engine_min_msgs").and_then(|v| v.as_integer()) {
                        state.engine_min_msgs = v as usize;
                    }
                    if let Some(v) = compression.get("engine_protect_first").and_then(|v| v.as_integer()) {
                        state.engine_protect_first = v as usize;
                    }
                    if let Some(v) = compression.get("engine_protect_last").and_then(|v| v.as_integer()) {
                        state.engine_protect_last = v as usize;
                    }
                    if let Some(v) = compression.get("tool_threshold").and_then(|v| v.as_integer()) {
                        state.tool_threshold = v as usize;
                    }
                    if let Some(v) = compression.get("terminal_threshold").and_then(|v| v.as_integer()) {
                        state.terminal_threshold = v as usize;
                    }
                }
                if let Some(defaults) = table.get("defaults").and_then(|v| v.as_table()) {
                    if let Some(v) = defaults.get("api_url").and_then(|v| v.as_str()) {
                        state.api_url = v.to_string();
                    }
                    if let Some(v) = defaults.get("model").and_then(|v| v.as_str()) {
                        state.model = v.to_string();
                    }
                }
            }
        }
    }

    let hid = alloc_handle(state);
    CString::new(hid.to_string()).unwrap().into_raw()
}

/// Destroy a runtime instance.
#[no_mangle]
pub extern "C" fn aphrodite_destroy(handle: *const c_char) {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }
        .to_string_lossy()
        .parse()
    {
        Ok(id) => id,
        Err(_) => return,
    };
    HANDLES.lock().unwrap().remove(&hid);
}

/// Classify content type. Returns JSON with type, lines, bytes.
#[no_mangle]
pub extern "C" fn aphrodite_classify(content: *const c_char) -> *mut c_char {
    let content = unsafe { CStr::from_ptr(content) }.to_string_lossy();
    let ct = transforms::detect(&content);
    CString::new(
        serde_json::json!({
            "type": ct.as_str(),
            "lines": content.lines().count(),
            "bytes": content.len(),
        })
        .to_string(),
    )
    .unwrap()
    .into_raw()
}

/// Compress and store in CCR. Returns {hash, type, size, preview, marker}.
#[no_mangle]
pub extern "C" fn aphrodite_compress(
    handle: *const c_char,
    content: *const c_char,
    type_hint: *const c_char,
) -> *mut c_char {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }.to_string_lossy().parse() {
        Ok(id) => id,
        Err(_) => return to_json_error("invalid handle"),
    };
    let content = unsafe { CStr::from_ptr(content) }.to_string_lossy();
    let hint = unsafe { CStr::from_ptr(type_hint) }.to_string_lossy();

    with_handle(hid, |state| {
        if content.is_empty() {
            return Err("empty content".into());
        }
        let ct = transforms::detect(&content);
        let type_str = if hint.is_empty() || hint == "text" {
            ct.as_str().to_string()
        } else {
            hint.to_string()
        };
        let hash = headroom_core::ccr::compute_key(content.as_bytes());
        state.inline_store_put(hash.clone(), content.to_string());
        let preview = build_preview(&type_str, &content);
        let marker = marker::ccr_marker(&hash, &type_str, content.len(), &preview, None, None, None);

        state.record_marker(state::MarkerEntry {
            hash: hash.clone(),
            ccr_type: type_str.clone(),
            size: content.len(),
            preview: preview.clone(),
            turn: state.turn_counter,
            center: None, meta: None,
        });

        Ok(serde_json::json!({
            "hash": hash, "type": type_str, "size": content.len(),
            "preview": preview, "marker": marker,
        }).to_string())
    })
}

/// Retrieve original content by hash. First checks inline store, then CCR.
#[no_mangle]
pub extern "C" fn aphrodite_retrieve(
    handle: *const c_char,
    hash: *const c_char,
) -> *mut c_char {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }.to_string_lossy().parse() {
        Ok(id) => id,
        Err(_) => return to_json_error("invalid handle"),
    };
    let hash = unsafe { CStr::from_ptr(hash) }.to_string_lossy();

    let mut handles = HANDLES.lock().unwrap();
    match handles.get_mut(&hid) {
        Some(state) => {
            // Check inline store first
            if let Some(content) = state.inline_store_get(&hash) {
                return CString::new(content).unwrap().into_raw();
            }
            to_json_error(&format!("hash not found: {}", hash))
        }
        None => to_json_error(&format!("invalid handle: {}", hid)),
    }
}

/// Transform tool output. Full plugin hook equivalent.
#[no_mangle]
pub extern "C" fn aphrodite_transform(
    handle: *const c_char,
    content: *const c_char,
    tool_name: *const c_char,
) -> *mut c_char {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }.to_string_lossy().parse() {
        Ok(id) => id,
        Err(_) => return to_json_error("invalid handle"),
    };
    let content = unsafe { CStr::from_ptr(content) }.to_string_lossy();
    let tool = unsafe { CStr::from_ptr(tool_name) }.to_string_lossy();

    with_handle(hid, |state| {
        let result = hooks::transform_tool_result(state, &content, &tool);
        Ok(result.to_string())
    })
}

/// Transform terminal output. Full plugin hook equivalent.
#[no_mangle]
pub extern "C" fn aphrodite_terminal(
    handle: *const c_char,
    content: *const c_char,
) -> *mut c_char {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }.to_string_lossy().parse() {
        Ok(id) => id,
        Err(_) => return to_json_error("invalid handle"),
    };
    let content = unsafe { CStr::from_ptr(content) }.to_string_lossy();

    with_handle(hid, |state| {
        let result = hooks::transform_terminal_output(state, &content);
        Ok(result.to_string())
    })
}

/// Start a new session — reset counters.
#[no_mangle]
pub extern "C" fn aphrodite_session_start(handle: *const c_char) -> *mut c_char {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }.to_string_lossy().parse() {
        Ok(id) => id,
        Err(_) => return to_json_error("invalid handle"),
    };

    with_handle(hid, |state| {
        let result = hooks::on_session_start(state);
        Ok(result.to_string())
    })
}

/// Get catalog of recent compressions.
/// mode: "toc" for compact, "full" for complete.
#[no_mangle]
pub extern "C" fn aphrodite_catalog(
    handle: *const c_char,
    mode: *const c_char,
) -> *mut c_char {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }.to_string_lossy().parse() {
        Ok(id) => id,
        Err(_) => return to_json_error("invalid handle"),
    };
    let mode = unsafe { CStr::from_ptr(mode) }.to_string_lossy();

    let handles = HANDLES.lock().unwrap();
    match handles.get(&hid) {
        Some(state) => {
            let items: Vec<serde_json::Value> = state
                .recent_markers
                .iter()
                .map(|m| {
                    if mode == "toc" {
                        serde_json::json!({
                            "hash": &m.hash[..12.min(m.hash.len())],
                            "type": m.ccr_type,
                            "size": m.size,
                            "preview": m.preview,
                        })
                    } else {
                        serde_json::json!({
                            "hash": m.hash,
                            "type": m.ccr_type,
                            "size": m.size,
                            "preview": m.preview,
                            "turn": m.turn,
                        })
                    }
                })
                .collect();

            CString::new(
                serde_json::json!({
                    "total": items.len(),
                    "items": items,
                    "turn": state.turn_counter,
                    "engine_enabled": state.context_engine_enabled,
                })
                .to_string(),
            )
            .unwrap()
            .into_raw()
        }
        None => to_json_error(&format!("invalid handle: {}", hid)),
    }
}

/// Get runtime stats.
#[no_mangle]
pub extern "C" fn aphrodite_stats(handle: *const c_char) -> *mut c_char {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }.to_string_lossy().parse() {
        Ok(id) => id,
        Err(_) => return to_json_error("invalid handle"),
    };

    let handles = HANDLES.lock().unwrap();
    match handles.get(&hid) {
        Some(state) => CString::new(
            serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "inline_entries": state.inline_store.len(),
                "markers": state.recent_markers.len(),
                "turn": state.turn_counter,
                "engine_enabled": state.context_engine_enabled,
                "threshold_pct": state.engine_threshold_pct,
                "tool_threshold": state.tool_threshold,
                "terminal_threshold": state.terminal_threshold,
                "model": state.model,
            })
            .to_string(),
        )
        .unwrap()
        .into_raw(),
        None => to_json_error(&format!("invalid handle: {}", hid)),
    }
}

/// Reload config from disk (re-reads aphrodite.toml).
#[no_mangle]
pub extern "C" fn aphrodite_reload(
    handle: *const c_char,
    config_path: *const c_char,
) -> *mut c_char {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }.to_string_lossy().parse() {
        Ok(id) => id,
        Err(_) => return to_json_error("invalid handle"),
    };
    let path = unsafe { CStr::from_ptr(config_path) }.to_string_lossy();

    let mut handles = HANDLES.lock().unwrap();
    match handles.get_mut(&hid) {
        Some(state) => {
            if !path.is_empty() {
                if let Ok(toml_str) = std::fs::read_to_string(path.as_ref()) {
                    if let Ok(table) = toml_str.parse::<toml::Table>() {
                        if let Some(c) = table.get("compression").and_then(|v| v.as_table()) {
                            if let Some(v) = c.get("context_engine").and_then(|v| v.as_bool()) {
                                state.context_engine_enabled = v;
                            }
                            if let Some(v) = c.get("engine_threshold_pct").and_then(|v| v.as_integer()) {
                                state.engine_threshold_pct = v as u64;
                            }
                            if let Some(v) = c.get("tool_threshold").and_then(|v| v.as_integer()) {
                                state.tool_threshold = v as usize;
                            }
                            if let Some(v) = c.get("terminal_threshold").and_then(|v| v.as_integer()) {
                                state.terminal_threshold = v as usize;
                            }
                        }
                    }
                }
            }
            CString::new(r#"{"status":"ok","action":"reloaded"}"#).unwrap().into_raw()
        }
        None => to_json_error(&format!("invalid handle: {}", hid)),
    }
}

/// Get config value.
#[no_mangle]
pub extern "C" fn aphrodite_config_get(
    handle: *const c_char,
    key: *const c_char,
) -> *mut c_char {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }.to_string_lossy().parse() {
        Ok(id) => id,
        Err(_) => return to_json_error("invalid handle"),
    };
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy();

    let handles = HANDLES.lock().unwrap();
    match handles.get(&hid) {
        Some(state) => {
            let val = match key.as_ref() {
                "api_url" => state.api_url.clone(),
                "model" => state.model.clone(),
                "engine_threshold_pct" => state.engine_threshold_pct.to_string(),
                "tool_threshold" => state.tool_threshold.to_string(),
                "terminal_threshold" => state.terminal_threshold.to_string(),
                "context_engine_enabled" => state.context_engine_enabled.to_string(),
                "catalog_mode" => state.catalog_mode.clone(),
                _ => return to_json_error(&format!("unknown key: {}", key)),
            };
            CString::new(val).unwrap().into_raw()
        }
        None => to_json_error(&format!("invalid handle: {}", hid)),
    }
}

/// Set config value at runtime.
#[no_mangle]
pub extern "C" fn aphrodite_config_set(
    handle: *const c_char,
    key: *const c_char,
    value: *const c_char,
) -> *mut c_char {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }.to_string_lossy().parse() {
        Ok(id) => id,
        Err(_) => return to_json_error("invalid handle"),
    };
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy();
    let value = unsafe { CStr::from_ptr(value) }.to_string_lossy();

    let mut handles = HANDLES.lock().unwrap();
    match handles.get_mut(&hid) {
        Some(state) => {
            match key.as_ref() {
                "model" => state.model = value.to_string(),
                "catalog_mode" => state.catalog_mode = value.to_string(),
                "engine_threshold_pct" => {
                    if let Ok(v) = value.parse() { state.engine_threshold_pct = v; }
                }
                "tool_threshold" => {
                    if let Ok(v) = value.parse() { state.tool_threshold = v; }
                }
                "terminal_threshold" => {
                    if let Ok(v) = value.parse() { state.terminal_threshold = v; }
                }
                "context_engine_enabled" => {
                    state.context_engine_enabled = value == "true" || value == "1";
                }
                _ => return to_json_error(&format!("unknown key: {}", key)),
            }
            CString::new(r#"{"status":"ok"}"#).unwrap().into_raw()
        }
        None => to_json_error(&format!("invalid handle: {}", hid)),
    }
}

/// Search stored content by keyword. Returns matching hashes + previews.
#[no_mangle]
pub extern "C" fn aphrodite_search(
    handle: *const c_char,
    query: *const c_char,
) -> *mut c_char {
    let hid: usize = match unsafe { CStr::from_ptr(handle) }.to_string_lossy().parse() {
        Ok(id) => id,
        Err(_) => return to_json_error("invalid handle"),
    };
    let query = unsafe { CStr::from_ptr(query) }.to_string_lossy().to_lowercase();

    let handles = HANDLES.lock().unwrap();
    match handles.get(&hid) {
        Some(state) => {
            let results: Vec<serde_json::Value> = state
                .recent_markers
                .iter()
                .filter(|m| m.preview.to_lowercase().contains(&query) || m.ccr_type.to_lowercase().contains(&query))
                .take(20)
                .map(|m| {
                    serde_json::json!({
                        "hash": &m.hash[..12.min(m.hash.len())],
                        "type": m.ccr_type,
                        "size": m.size,
                        "preview": m.preview,
                    })
                })
                .collect();

            CString::new(
                serde_json::json!({"total": results.len(), "results": results}).to_string(),
            )
            .unwrap()
            .into_raw()
        }
        None => to_json_error(&format!("invalid handle: {}", hid)),
    }
}

/// Dylib version.
#[no_mangle]
pub extern "C" fn aphrodite_version() -> *mut c_char {
    CString::new(env!("CARGO_PKG_VERSION")).unwrap().into_raw()
}

/// Free a string returned by any aphrodite_* function. Null-safe.
#[no_mangle]
pub extern "C" fn aphrodite_free_string(s: *mut c_char) {
    if s.is_null() { return; }
    unsafe { let _ = CString::from_raw(s); }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn to_json_error(msg: &str) -> *mut c_char {
    CString::new(serde_json::json!({"error": msg}).to_string()).unwrap().into_raw()
}

pub(crate) fn build_preview(type_str: &str, content: &str) -> String {
    let lines = content.lines().count();
    let bytes = content.len();

    match type_str {
        "build" | "build_output" | "build_error" => {
            let errors = content.matches("error").count();
            let warns = content.matches("warning").count();
            format!("[{}:{}E {}W {}L]", type_str, errors, warns, lines)
        }
        "diff" => {
            let files = content.matches("diff --git").count();
            let adds = content.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count();
            let dels = content.lines().filter(|l| l.starts_with('-') && !l.starts_with("---")).count();
            format!("[diff:{}F +{}/-{} {}L]", files, adds, dels, lines)
        }
        "source_code" => {
            let fns = content.matches("fn ").count() + content.matches("def ").count();
            format!("[code:{}fns {}L]", fns, lines)
        }
        "search" => {
            let hits = content.lines().filter(|l| l.contains(':')).count();
            format!("[search:{}hits {}L]", hits, lines)
        }
        "json_array" => {
            let items = content.matches("{\"").count();
            format!("[json:{}items {}L]", items, lines)
        }
        _ => format!("[{}:{}L {}B]", type_str, lines, bytes),
    }
}
