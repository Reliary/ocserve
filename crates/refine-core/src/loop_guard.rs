//! Loop intelligence (DIFFERENTIATION.md D1 / P1b).
//!
//! Detectors over the current prompt's tool-call window. Upstream parity:
//! `repeat` fires the SAME permission name upstream uses (`doom_loop`,
//! processor.ts:29,356-383) when the current call plus the previous two are
//! identical (tool + input). `oscillation`/`spiral` are our extensions and
//! ride the same permission flow distinguished only by additive
//! `metadata.class` — never a new SSE event type.
//!
//! All logic here is pure (window in, class out) so every detector gets a
//! unit test with a planted negative control. Response strategy (permission
//! ask, metrics) lives in prompt.rs.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub tool: String,
    /// normalized input JSON (serde re-serialization of the parsed args —
    /// mirrors upstream's JSON.stringify equality check)
    pub input_key: String,
    pub out_hash: u64,
    pub out_len: usize,
    pub err: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// 3 consecutive identical (tool, input) — upstream doom_loop parity
    Repeat,
    /// A,B,A,B… alternating pair — extension
    Oscillation,
    /// two consecutive >= SPIRAL_FACTOR output growths — extension
    Spiral,
    /// 3 consecutive error outputs — metric-only (no ask: iterating on a
    /// failing test is legitimate work, asking would be fatigue)
    ErrorStorm,
}

impl Class {
    pub fn as_str(&self) -> &'static str {
        match self {
            Class::Repeat => "repeat",
            Class::Oscillation => "oscillation",
            Class::Spiral => "spiral",
            Class::ErrorStorm => "error_storm",
        }
    }
}

/// spiral: both consecutive growth steps must exceed the factor and the
/// base must be material (>=1 KiB) so tiny outputs never ask
pub const SPIRAL_FACTOR: f64 = 2.5;
pub const SPIRAL_MIN_BASE: usize = 1024;
/// oscillation needs at least this many window entries (A,B,A + current B)
const OSC_MIN: usize = 3;

/// Kill switch: REFINE_LOOP_GUARD=0|off → detectors still compute metrics
/// but never ask (env-tunable per SRE; no config surface until proven).
pub fn asks_enabled() -> bool {
    !matches!(
        std::env::var("REFINE_LOOP_GUARD")
            .unwrap_or_default()
            .as_str(),
        "0" | "off" | "false"
    )
}

/// Normalized input key for a raw tool-call arguments JSON string.
/// Invalid JSON degrades to the raw string (still a valid equality key).
pub fn input_key(arguments: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(v) => v.to_string(),
        Err(_) => arguments.to_string(),
    }
}

pub fn output_hash(output: &str) -> u64 {
    let mut h = DefaultHasher::new();
    output.hash(&mut h);
    h.finish()
}

pub fn entry(tool: &str, arguments: &str, output: &str, err: bool) -> Entry {
    Entry {
        tool: tool.to_string(),
        input_key: input_key(arguments),
        out_hash: output_hash(output),
        out_len: output.len(),
        err,
    }
}

/// Pre-exec detection: fires on the call ABOUT to run against the window of
/// already-executed calls in this compaction round.
pub fn check_pre(win: &[Entry], tool: &str, input_key: &str) -> Option<Class> {
    // repeat (upstream parity): current + previous two identical
    if win.len() >= 2 {
        let a = &win[win.len() - 2];
        let b = &win[win.len() - 1];
        if a.tool == tool && b.tool == tool && a.input_key == input_key && b.input_key == input_key
        {
            return Some(Class::Repeat);
        }
    }
    // oscillation: win = [A,B,A] and current = B (distinct pair, alternating)
    if win.len() >= OSC_MIN {
        let n = win.len();
        let a1 = &win[n - 3];
        let b1 = &win[n - 2];
        let a2 = &win[n - 1];
        let key_a = (a1.tool.clone(), a1.input_key.clone());
        let key_b = (b1.tool.clone(), b1.input_key.clone());
        let current = (tool.to_string(), input_key.to_string());
        if key_a == (a2.tool.clone(), a2.input_key.clone()) && key_a != key_b && current == key_b {
            return Some(Class::Oscillation);
        }
    }
    None
}

/// Post-exec detection: fires after the newest entry is appended.
pub fn check_post(win: &[Entry]) -> Option<Class> {
    // spiral: last three entries, two consecutive >= FACTOR growths
    if win.len() >= 3 {
        let n = win.len();
        let b2 = &win[n - 3].out_len;
        let b1 = &win[n - 2].out_len;
        let b0 = &win[n - 1].out_len;
        if *b2 >= SPIRAL_MIN_BASE
            && (*b1 as f64) >= (*b2 as f64) * SPIRAL_FACTOR
            && (*b0 as f64) >= (*b1 as f64) * SPIRAL_FACTOR
        {
            return Some(Class::Spiral);
        }
    }
    // error storm: three consecutive error outputs
    if win.len() >= 3 {
        let n = win.len();
        if win[n - 1].err && win[n - 2].err && win[n - 3].err {
            return Some(Class::ErrorStorm);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(tool: &str, input: &str, len: usize, err: bool) -> Entry {
        Entry {
            tool: tool.into(),
            input_key: input.into(),
            out_hash: 0,
            out_len: len,
            err,
        }
    }

    #[test]
    fn input_key_normalizes_whitespace_and_key_order_is_raw() {
        assert_eq!(
            input_key(r#"{"a":1,"b":2}"#),
            input_key(r#" { "a" : 1 , "b" : 2 } "#)
        );
        // serde preserves insertion order (preserve_order feature) — same
        // semantic object with different serialized order stays distinct,
        // matching upstream's JSON.stringify comparison semantics
        assert_ne!(input_key(r#"{"a":1,"b":2}"#), input_key(r#"{"b":2,"a":1}"#));
    }

    #[test]
    fn repeat_negative_control_fires_on_third_not_second() {
        let w = vec![e("bash", "k", 10, false)];
        assert_eq!(
            check_pre(&w, "bash", "k"),
            None,
            "2nd identical must NOT fire"
        );
        let w = vec![e("bash", "k", 10, false), e("bash", "k", 10, false)];
        assert_eq!(
            check_pre(&w, "bash", "k"),
            Some(Class::Repeat),
            "3rd identical fires (upstream parity)"
        );
        // different input breaks the streak
        let w = vec![e("bash", "k", 10, false), e("bash", "k2", 10, false)];
        assert_eq!(check_pre(&w, "bash", "k"), None, "changed input resets");
    }

    #[test]
    fn oscillation_fires_abab_not_aba_or_abc() {
        // A,B,A + current B → fire
        let w = vec![
            e("bash", "x", 1, false),
            e("bash", "y", 1, false),
            e("bash", "x", 1, false),
        ];
        assert_eq!(check_pre(&w, "bash", "y"), Some(Class::Oscillation));
        // A,B,C → no
        let w = vec![
            e("bash", "x", 1, false),
            e("bash", "y", 1, false),
            e("bash", "z", 1, false),
        ];
        assert_eq!(
            check_pre(&w, "bash", "w"),
            None,
            "non-alternating must not fire"
        );
        // A,B,A + current A (not alternating) → no
        let w = vec![
            e("bash", "x", 1, false),
            e("bash", "y", 1, false),
            e("bash", "x", 1, false),
        ];
        assert_eq!(check_pre(&w, "bash", "x"), None);
    }

    #[test]
    fn spiral_needs_two_consecutive_growth_steps_over_material_base() {
        // 1KiB → 3KiB → 8KiB: both steps >= 2.5x → fire
        let w = vec![
            e("bash", "a", 1024, false),
            e("bash", "b", 3072, false),
            e("bash", "c", 8000, false),
        ];
        assert_eq!(check_post(&w), Some(Class::Spiral));
        // single jump then plateau → no (negative control: growing-but-stable)
        let w = vec![
            e("bash", "a", 1024, false),
            e("bash", "b", 4096, false),
            e("bash", "c", 4100, false),
        ];
        assert_eq!(check_post(&w), None, "one jump must not fire");
        // growth below material base → no (tiny outputs)
        let w = vec![
            e("bash", "a", 10, false),
            e("bash", "b", 40, false),
            e("bash", "c", 200, false),
        ];
        assert_eq!(check_post(&w), None, "sub-1KiB base must not fire");
    }

    #[test]
    fn error_storm_three_consecutive_errors() {
        let w = vec![
            e("bash", "a", 5, true),
            e("bash", "b", 5, true),
            e("bash", "c", 5, true),
        ];
        assert_eq!(check_post(&w), Some(Class::ErrorStorm));
        let w = vec![
            e("bash", "a", 5, true),
            e("bash", "b", 5, false),
            e("bash", "c", 5, true),
        ];
        assert_eq!(check_post(&w), None, "recovered run breaks the storm");
    }

    #[test]
    fn kill_switch_env() {
        // env mutation is unsafe in edition 2024; test-local and restored
        unsafe {
            std::env::remove_var("REFINE_LOOP_GUARD");
        }
        assert!(asks_enabled(), "default must be enabled");
        unsafe {
            std::env::set_var("REFINE_LOOP_GUARD", "0");
        }
        assert!(!asks_enabled(), "REFINE_LOOP_GUARD=0 disables asks");
        unsafe {
            std::env::remove_var("REFINE_LOOP_GUARD");
        }
    }
}
