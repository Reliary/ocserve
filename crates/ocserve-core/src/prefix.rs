//! Provider prefix-change attribution.
//!
//! Whether the provider's KV/prefix cache can be reused between two requests
//! depends on the bytes BEFORE the conversation tail: the system prompt and the
//! tool schemas. `build_request` is ours, so we can hash exactly those bytes
//! and report when they change.
//!
//! This is observability, not an optimisation: nothing here changes a request.
//! It exists because a silent prefix change (a plugin adding a tool, a system
//! transform becoming non-deterministic, a config reload altering the agent)
//! costs money and latency and is otherwise invisible.
//!
//! Cost: two SHA-256 passes over the system prompt and the tool schema array
//! per model round. The tool array is the frozen 26 KB capture, so this is
//! tens of microseconds and allocates nothing beyond the hasher.
//!
//! State is per session, bounded, and in-memory only. Process restart resets
//! it, which is correct: the first request after a restart has nothing to
//! compare against.

use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Hard cap on tracked sessions. A busy server with many sessions would
/// otherwise grow without bound (AGENTS §2.3).
const MAX_TRACKED: usize = 1024;

/// Hard cap on the total bytes of session-id keys. Session ids are
/// client/import-supplied strings, so an entry count alone is not a bound:
/// 1024 ids of 1 MiB would be 1 GiB. On overflow the OLDEST entries are
/// dropped (rather than the whole table, which used to disable attribution
/// globally on a busy box).
const MAX_KEY_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    system: [u8; 16],
    tools: [u8; 16],
}

fn system_fp(system: &[String]) -> [u8; 16] {
    // Length-prefix each entry so ["ab","c"] and ["a","bc"] differ.
    let mut h = Sha256::new();
    for s in system {
        h.update((s.len() as u64).to_le_bytes());
        h.update(s.as_bytes());
    }
    let out = h.finalize();
    let mut r = [0u8; 16];
    r.copy_from_slice(&out[..16]);
    r
}

fn tools_fp(tools: Option<&[serde_json::Value]>) -> [u8; 16] {
    match tools {
        None => [0u8; 16],
        Some(list) => {
            let mut h = Sha256::new();
            h.update((list.len() as u64).to_le_bytes());
            for t in list {
                // `serde_json` preserves insertion order, and the tool list is
                // built deterministically, so this is stable across requests.
                let bytes = serde_json::to_vec(t).unwrap_or_default();
                h.update((bytes.len() as u64).to_le_bytes());
                h.update(&bytes);
            }
            let out = h.finalize();
            let mut r = [0u8; 16];
            r.copy_from_slice(&out[..16]);
            r
        }
    }
}

struct State {
    map: HashMap<String, Fingerprint>,
    /// Insertion order, oldest first (bounded by MAX_TRACKED).
    order: std::collections::VecDeque<String>,
    key_bytes: usize,
}

fn table() -> &'static Mutex<State> {
    static T: OnceLock<Mutex<State>> = OnceLock::new();
    T.get_or_init(|| {
        Mutex::new(State {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
            key_bytes: 0,
        })
    })
}

fn evict_oldest(st: &mut State) {
    if let Some(old) = st.order.pop_front()
        && st.map.remove(&old).is_some()
    {
        st.key_bytes = st.key_bytes.saturating_sub(old.len());
    }
}

/// Compare this request's prefix with the session's previous one and count
/// changes. The first observation for a session establishes the baseline and
/// emits nothing.
pub(crate) fn observe(
    session_id: &str,
    system: &[String],
    tools: Option<&[serde_json::Value]>,
) -> Option<(bool, bool)> {
    let now = Fingerprint {
        system: system_fp(system),
        tools: tools_fp(tools),
    };
    let mut st = table().lock();
    let previous = st.map.get(session_id).copied();
    if previous.is_none() {
        while !st.order.is_empty()
            && (st.map.len() >= MAX_TRACKED
                || st.key_bytes.saturating_add(session_id.len()) > MAX_KEY_BYTES)
        {
            evict_oldest(&mut st);
        }
        st.order.push_back(session_id.to_string());
        st.key_bytes = st.key_bytes.saturating_add(session_id.len());
    }
    st.map.insert(session_id.to_string(), now);
    let prev = previous?;
    let system_changed = prev.system != now.system;
    let tools_changed = prev.tools != now.tools;
    if system_changed {
        ocserve_metrics::labeled_counter(
            "ocserve_provider_prefix_changed_total",
            "part=\"system\"",
            1,
        );
    }
    if tools_changed {
        ocserve_metrics::labeled_counter(
            "ocserve_provider_prefix_changed_total",
            "part=\"tools\"",
            1,
        );
    }
    Some((system_changed, tools_changed))
}

/// Forget a session (session delete / test isolation).
pub fn forget(session_id: &str) {
    let mut st = table().lock();
    if st.map.remove(session_id).is_some() {
        st.key_bytes = st.key_bytes.saturating_sub(session_id.len());
        st.order.retain(|k| k != session_id);
    }
}

/// Test-only: make the tracker empty so counters are deterministic.
#[cfg(test)]
pub(crate) fn clear() {
    let mut st = table().lock();
    st.map.clear();
    st.order.clear();
    st.key_bytes = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The tracker is process-global, so these tests must not interleave.
    fn guard() -> parking_lot::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock()
    }

    fn sys(s: &str) -> Vec<String> {
        vec![s.to_string()]
    }

    #[test]
    fn first_observation_is_a_baseline() {
        let _g = guard();
        clear();
        assert_eq!(observe("s1", &sys("a"), None), None);
    }

    #[test]
    fn identical_prefix_reports_nothing() {
        let _g = guard();
        clear();
        let t = vec![json!({"function": {"name": "read"}})];
        assert_eq!(observe("s2", &sys("a"), Some(&t)), None);
        assert_eq!(observe("s2", &sys("a"), Some(&t)), Some((false, false)));
    }

    #[test]
    fn system_change_is_attributed_to_system() {
        let _g = guard();
        clear();
        let t = vec![json!({"function": {"name": "read"}})];
        observe("s3", &sys("a"), Some(&t));
        assert_eq!(
            observe("s3", &sys("b"), Some(&t)),
            Some((true, false)),
            "a system-only change must not be blamed on tools"
        );
    }

    #[test]
    fn tool_change_is_attributed_to_tools() {
        let _g = guard();
        clear();
        let t1 = vec![json!({"function": {"name": "read"}})];
        let t2 = vec![
            json!({"function": {"name": "read"}}),
            json!({"function": {"name": "grep"}}),
        ];
        observe("s4", &sys("a"), Some(&t1));
        assert_eq!(
            observe("s4", &sys("a"), Some(&t2)),
            Some((false, true)),
            "adding a tool must be attributed to tools"
        );
    }

    #[test]
    fn sessions_are_independent() {
        let _g = guard();
        clear();
        observe("s5a", &sys("a"), None);
        observe("s5b", &sys("zzz"), None);
        // Different sessions must not be compared against each other.
        assert_eq!(observe("s5a", &sys("a"), None), Some((false, false)));
        assert_eq!(observe("s5b", &sys("zzz"), None), Some((false, false)));
    }

    #[test]
    fn field_order_inside_a_tool_matters_for_the_bytes() {
        // The hash covers serialized bytes, which is what the provider sees.
        // Two objects with the same keys in a different order are different
        // requests, so they must not compare equal.
        let _g = guard();
        clear();
        let a = vec![json!({"a": 1, "b": 2})];
        let b = vec![json!({"b": 2, "a": 1})];
        observe("s6", &sys("x"), Some(&a));
        assert_eq!(observe("s6", &sys("x"), Some(&b)), Some((false, true)));
    }

    #[test]
    fn entry_count_is_capped() {
        let _g = guard();
        clear();
        for i in 0..(MAX_TRACKED + 10) {
            observe(&format!("cap{i}"), &sys("a"), None);
        }
        let st = table().lock();
        assert!(st.map.len() <= MAX_TRACKED, "entry cap");
        assert!(st.key_bytes <= MAX_KEY_BYTES, "key bytes cap");
    }

    #[test]
    fn huge_session_ids_cannot_grow_the_table_without_bound() {
        let _g = guard();
        clear();
        let big = "s".repeat(100_000);
        for i in 0..20 {
            observe(&format!("{big}{i}"), &sys("a"), None);
        }
        let st = table().lock();
        assert!(st.key_bytes <= MAX_KEY_BYTES, "key bytes: {}", st.key_bytes);
    }

    #[test]
    fn overflow_evicts_oldest_not_everything() {
        let _g = guard();
        clear();
        observe("keep", &sys("a"), None);
        for i in 0..(MAX_TRACKED + 5) {
            observe(&format!("cap{i}"), &sys("a"), None);
        }
        // The newest observations are tracked; the first one may be evicted.
        let st = table().lock();
        assert!(st.map.contains_key(&format!("cap{}", MAX_TRACKED + 4)));
        assert!(st.map.len() <= MAX_TRACKED);
    }

    #[test]
    fn forget_resets_the_baseline() {
        let _g = guard();
        clear();
        observe("s7", &sys("a"), None);
        forget("s7");
        assert_eq!(observe("s7", &sys("b"), None), None);
    }
}
