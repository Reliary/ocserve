#![no_main]
//! Provider stream assembler fuzz: arbitrary tool-call deltas must never
//! panic, must stay bounded (MAX_TOOL_CALL_SLOTS), and a finish() must not
//! blow up on sparse/gapped slot indices. Guards the OpenAI streaming
//! protocol accumulator (a hostile index once allocated GB).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Interpret bytes as a sequence of (index, delta) operations.
    let mut asm = ocserve_llm::ToolCallAssembler::default();
    let mut i = 0usize;
    while i < data.len() {
        let index = data[i] as usize;
        let len = (data.get(i + 1).copied().unwrap_or(0) as usize) % 64;
        let start = (i + 2).min(data.len());
        let end = (start + len).min(data.len());
        let delta = String::from_utf8_lossy(&data[start..end]).into_owned();
        let id = if data.get(i + 1).copied().unwrap_or(0) & 1 == 0 {
            Some(format!("call_{index}"))
        } else {
            None
        };
        let name = if data.get(i + 1).copied().unwrap_or(0) & 2 == 0 {
            Some("tool".to_string())
        } else {
            None
        };
        asm.push(index, id, name, delta);
        assert!(
            asm.slot_count() <= ocserve_llm::MAX_TOOL_CALL_SLOTS,
            "assembler exceeded its slot bound"
        );
        i = end.max(i + 2);
    }
    let calls = asm.finish();
    // Every emitted call has both id and name (filter_map requires Some).
    for c in calls {
        assert!(!c.id.is_empty());
        assert!(!c.name.is_empty());
    }
});
