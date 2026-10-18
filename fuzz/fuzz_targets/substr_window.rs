#![no_main]
//! substr/snippet window-math fuzz: arbitrary (text, needle) pairs through the
//! `snippet` window must never panic on char boundaries (the 2026-10-11 panic
//! class: `String::truncate` mid-char aborts the process; here lowercasing
//! shifts byte offsets, so every slice must be boundary-clamped).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // First byte splits text/needle on a char boundary.
    let split = data.first().copied().unwrap_or(0) as usize % (data.len().max(1));
    let (a, b) = data.split_at(split.min(data.len()));
    let Ok(text) = std::str::from_utf8(a) else {
        return;
    };
    let Ok(needle) = std::str::from_utf8(b) else {
        return;
    };
    // Empty needle is a valid input (matches at 0); must not panic.
    let out = ocserve_store::snippet(text, needle);
    // Output is valid UTF-8 (String) and bounded: the window never exceeds
    // the source text plus at most two 3-byte ellipses.
    assert!(out.len() <= text.len() + 6, "snippet grows text beyond ellipses");
});
