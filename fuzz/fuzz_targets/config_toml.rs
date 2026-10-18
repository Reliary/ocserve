#![no_main]
//! Config TOML fuzz: arbitrary bytes through the `ocserve.toml` decoder must
//! never panic. `deny_unknown_fields` + typed fields mean bad input returns
//! Err, never a panic or an unbounded allocation.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // Must never panic; Ok/Err are both fine. The parse is pure (no fs).
    let _ = ocserve::server_config::parse(text);
});
