#![no_main]
//! Splice (zero-parse JSON passthrough) fuzz: `compact_splice` may return
//! false (fall back to DOM) but must never panic, and when it returns true the
//! output must be valid JSON. This is the L1 byte-parity parser.
use libfuzzer_sys::fuzz_target;
use ocserve_store::splice::{compact_splice, SpliceScratch};

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else { return };
    let mut out = Vec::new();
    let mut scratch = SpliceScratch::default();
    if compact_splice(src, "id", "ses", None, &mut out, &mut scratch) {
        // Accepting means the output is a JSON object; it must parse.
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out) {
            let _ = v;
        } else {
            panic!("compact_splice accepted but produced invalid JSON: {:?}", String::from_utf8_lossy(&out));
        }
    }
});
