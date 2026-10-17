#![no_main]
//! MCP JSON-RPC fuzz: arbitrary JSON through the MCP config parser and the
//! trust scanner (both pure, both parse client/server-supplied data) must
//! never panic. The bounded-read path in the live pump is exercised
//! separately; this covers the decode/sanitize surface.
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    let Ok(v) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    // MCP server config map → cfg list (arbitrary values, missing fields).
    let _ = ocserve_mcp::parse_config(&v);
    // Trust scan over arbitrary tool metadata (bounded internally; the
    // scanner lowercases + contains-matches, must never panic).
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = ocserve_mcp::trust::scan_text(text);
        let _ = ocserve_mcp::trust::scan_tool(text, text, text);
    }
});
