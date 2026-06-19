//! headroom-ffi: Stable C ABI for the Aphrodite compression engine.
//!
//! Every function: input `*const c_char` → output `*mut c_char`.
//! Caller must free with `aphrodite_free_string`. Errors return JSON `{"error":"..."}`.
//!
//! Language-agnostic: load via Python ctypes, Node ffi-napi, Go cgo, etc.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Mutex;

use headroom_core::ccr;
use headroom_core::transforms;

// ── Global CCR store ─────────────────────────────────────────────────
static CCR_STORE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn store() -> std::sync::MutexGuard<'static, Option<HashMap<String, String>>> {
    let mut g = CCR_STORE.lock().unwrap();
    if g.is_none() {
        *g = Some(HashMap::new());
    }
    g
}

// ── Public C ABI ─────────────────────────────────────────────────────

/// Classify content. Returns JSON: `{"type":"source_code","confidence":0.9,"lines":42,"bytes":1234}`
#[no_mangle]
pub extern "C" fn aphrodite_classify(content: *const c_char) -> *mut c_char {
    let content = unsafe { CStr::from_ptr(content) }.to_string_lossy();
    let ct = transforms::detect(&content);
    let result = serde_json::json!({
        "type": ct.as_str(),
        "lines": content.lines().count(),
        "bytes": content.len(),
    });
    CString::new(result.to_string()).unwrap().into_raw()
}

/// Compress and store in CCR. Returns: `{"hash":"abc123...","type":"build","size":1234,"preview":"[build:0E 2W 142L]"}`
#[no_mangle]
pub extern "C" fn aphrodite_compress(
    content: *const c_char,
    type_hint: *const c_char,
) -> *mut c_char {
    let content = unsafe { CStr::from_ptr(content) }.to_string_lossy();
    let hint = unsafe { CStr::from_ptr(type_hint) }.to_string_lossy();

    if content.is_empty() {
        return to_json_error("empty content");
    }

    let ct = transforms::detect(&content);
    let type_str = if hint.is_empty() || hint == "text" {
        ct.as_str().to_string()
    } else {
        hint.to_string()
    };

    let hash = ccr::compute_key(content.as_bytes());

    {
        let mut s = store();
        if let Some(ref mut map) = *s {
            map.insert(hash.clone(), content.to_string());
        }
    }

    let preview = build_preview(&type_str, &content);

    let result = serde_json::json!({
        "hash": hash,
        "type": type_str,
        "size": content.len(),
        "preview": preview,
    });

    CString::new(result.to_string()).unwrap().into_raw()
}

/// Retrieve original content by hash.
#[no_mangle]
pub extern "C" fn aphrodite_retrieve(hash: *const c_char) -> *mut c_char {
    let hash = unsafe { CStr::from_ptr(hash) }.to_string_lossy();
    let s = store();
    if let Some(ref map) = *s {
        if let Some(content) = map.get(hash.as_ref()) {
            return CString::new(content.clone()).unwrap().into_raw();
        }
    }
    to_json_error(&format!("hash not found: {}", hash))
}

/// Dylib version.
#[no_mangle]
pub extern "C" fn aphrodite_version() -> *mut c_char {
    CString::new(env!("CARGO_PKG_VERSION")).unwrap().into_raw()
}

/// Free a string from any aphrodite_* function. Null-safe.
#[no_mangle]
pub extern "C" fn aphrodite_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    unsafe {
        let _ = CString::from_raw(s);
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn to_json_error(msg: &str) -> *mut c_char {
    let json = serde_json::json!({"error": msg});
    CString::new(json.to_string()).unwrap().into_raw()
}

fn build_preview(type_str: &str, content: &str) -> String {
    let lines = content.lines().count();
    let bytes = content.len();

    match type_str {
        "build" | "build_output" | "build_error" => {
            let errors = content.matches("error").filter(|_| true).count();
            let warns = content.matches("warning").filter(|_| true).count();
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
        "terminal" | "json_array" | "html" | "text" => {
            format!("[{}:{}L {}B]", type_str, lines, bytes)
        }
        _ => {
            format!("[{}:{}L {}B]", type_str, lines, bytes)
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn call1(f: unsafe extern "C" fn(*const c_char) -> *mut c_char, input: &str) -> String {
        let c = CString::new(input).unwrap();
        let p = unsafe { f(c.as_ptr()) };
        let r = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        aphrodite_free_string(p);
        r
    }

    fn call2(
        f: unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_char,
        a: &str,
        b: &str,
    ) -> String {
        let ca = CString::new(a).unwrap();
        let cb = CString::new(b).unwrap();
        let p = unsafe { f(ca.as_ptr(), cb.as_ptr()) };
        let r = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        aphrodite_free_string(p);
        r
    }

    #[test]
    fn test_version() {
        let p = aphrodite_version();
        let v = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        aphrodite_free_string(p);
        assert!(!v.contains("error"), "got: {}", v);
    }

    #[test]
    fn test_classify_rust() {
        let r = call1(aphrodite_classify, "pub fn main() {\n    println!(\"hello\");\n}\n");
        assert!(r.contains("source_code") || r.contains("text"), "got: {}", r);
    }

    #[test]
    fn test_classify_diff() {
        let r = call1(aphrodite_classify, "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,4 @@\n+fn new() {}\n");
        assert!(r.contains("diff"), "got: {}", r);
    }

    #[test]
    fn test_compress_retrieve_roundtrip() {
        let content = "fn hello() -> &'static str { \"world\" }";
        let r = call2(aphrodite_compress, content, "source_code");
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        let hash = v["hash"].as_str().unwrap();

        let retrieved = call1(aphrodite_retrieve, hash);
        assert_eq!(retrieved, content);
    }

    #[test]
    fn test_compress_empty() {
        let r = call2(aphrodite_compress, "", "text");
        assert!(r.contains("empty content"));
    }

    #[test]
    fn test_retrieve_missing() {
        let r = call1(aphrodite_retrieve, "deadbeef1234");
        assert!(r.contains("not found"));
    }

    #[test]
    fn test_free_null() {
        aphrodite_free_string(std::ptr::null_mut());
    }
}
