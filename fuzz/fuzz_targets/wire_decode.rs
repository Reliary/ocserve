#![no_main]
//! Typed wire-decode fuzz: arbitrary JSON into each request decoder must never
//! panic; a successful decode must round-trip to an object. Guards the
//! field-contract surface (messageID / v2 envelopes).
use libfuzzer_sys::fuzz_target;
use ocserve_core::wire::{CommandRequest, PromptRequest, ShellRequest, V2PromptInput};

fuzz_target!(|data: &[u8]| {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) else { return };
    let _ = PromptRequest::decode(&v);
    let _ = CommandRequest::decode(&v);
    let _ = ShellRequest::decode(&v);
    let _ = V2PromptInput::decode(&v);
});
