#![no_main]
//! Event decoder fuzz: arbitrary JSON properties through the event
//! normalization + v2-twin choke points must never panic (the 2026-10-09
//! TUI crash class was a missing required field — this explores the shape
//! space; normalize_props is the single emitter choke).
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    let Ok(props) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    // Arbitrary event-type strings exercise every branch (base+".1" strip,
    // part presence, unmapped fall-through).
    for ty in [
        "message.part.updated",
        "message.part.updated.1",
        "message.updated",
        "session.status",
        "session.idle",
        "unknown.type",
        "",
    ] {
        let normalized = ocserve_core::event::normalize_props(ty, props.clone());
        // Idempotence: normalizing an already-normalized payload is a no-op.
        let again = ocserve_core::event::normalize_props(ty, normalized.clone());
        assert_eq!(normalized, again, "normalize_props must be idempotent");
        let _ = ocserve_core::event::v2_twin(ty, &normalized);
        let _ = ocserve_core::event::frame("/tmp", ty, props.clone());
    }
});
