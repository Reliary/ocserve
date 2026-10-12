#![no_main]
//! SSE line-parser fuzz: arbitrary bytes split at arbitrary boundaries must
//! never panic and must consume without unbounded growth. Feeds the same
//! public path the live stream parser uses (chunk-split invariance was the
//! bug class behind the `..=pos` finding — this explores the input space).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut p = ocserve_llm::SseLineParser::new();
    // whole-buffer feed
    p.push(data);
    p.finish();
    while p.pop().is_some() {}

    // adversarial chunk splits (every cut point for short inputs)
    if data.len() <= 64 {
        for cut in 0..=data.len() {
            let mut p = ocserve_llm::SseLineParser::new();
            p.push(&data[..cut]);
            p.push(&data[cut..]);
            p.finish();
            while p.pop().is_some() {}
        }
    }
});
