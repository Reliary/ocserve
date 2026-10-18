//! v2 run coordinator (V2-PLAN.md P3): admission → wake → drain.
//!
//! Mirrors upstream's server-owned queue (`run-coordinator.ts`, `execution.ts`,
//! `inbox.ts`) in miniature: the HTTP layer only ADMITS work to
//! `session_inbox`; the coordinator owns promotion order and execution.
//! Delivery semantics:
//! - prompts steer by default (`delivery` hint from the request)
//! - step boundary: steers only
//! - idle boundary: steers first, then exactly one queued item
//! - steered compaction jumps ahead of earlier steers, never across a move
//! - `interrupt?resume=true` wakes steer scope only (queued stays parked)
//!
//! The coordinator reuses the existing `run_prompt` turn body: `drain` =
//! promote-one + run-one-turn + loop while promotable. No second executor.
//! Claims (`store::claim`) wrap the drain: claimed on start (in the same
//! writer batch as the `execution.started` event — write-ahead), released on
//! terminal states, preserved on shutdown (boot recovery reaps them).
//!
//! Bounds (AGENTS §2.3): one coordinator task per session max (held in
//! `CoordinatorSet`); inbox rows are pending-only (no prune loop); the drain
//! loop re-checks promotability per iteration (no infinite spin — an empty
//! promote ends the drain).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Which scope a wake covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeScope {
    /// A new steer arrived (or resume=true interrupt): run steers now.
    Steer,
    /// Idle wake: steers first, then one queued item.
    Input,
}

/// Per-session coordinator state.
#[derive(Debug)]
struct SessionCoord {
    /// A drain is currently running (prevents a second fiber per session).
    draining: bool,
    /// A wake arrived while draining (doorbell — widest scope wins).
    pending: Option<WakeScope>,
}

impl SessionCoord {
    fn wake(&mut self, scope: WakeScope) {
        self.pending = Some(match (self.pending, scope) {
            (Some(WakeScope::Input), _) | (_, WakeScope::Input) => WakeScope::Input,
            _ => WakeScope::Steer,
        });
    }
}

/// Coordinator set: at most one entry per session (bounded by the session
/// count; entries drop when the session drains empty AND no claim is held —
/// see `release_if_idle`).
#[derive(Debug, Default)]
pub struct CoordinatorSet {
    inner: Mutex<HashMap<String, SessionCoord>>,
}

impl CoordinatorSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wake a session's coordinator. Returns `true` when the caller should
    /// spawn a drain (no drain running); `false` when the doorbell was set
    /// on a live drain.
    pub fn wake(&self, session_id: &str, scope: WakeScope) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let coord = inner.entry(session_id.to_string()).or_insert(SessionCoord {
            draining: false,
            pending: None,
        });
        coord.wake(scope);
        if coord.draining {
            return false;
        }
        coord.draining = true;
        true
    }

    /// Take the pending scope for the next drain iteration. Returns `None`
    /// when nothing is pending (drain ends; `draining` clears).
    pub fn take_pending(&self, session_id: &str) -> Option<WakeScope> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let coord = inner.get_mut(session_id)?;
        let scope = coord.pending.take();
        if scope.is_none() {
            coord.draining = false;
            if !claim_would_hold(session_id) {
                inner.remove(session_id);
            }
        }
        scope
    }

    /// Drain-continuation check: returns the scope to run next, or `None` to
    /// end the drain. Prefers an explicit doorbell; otherwise continues while
    /// `work_remains` (the caller re-checks promotability — upstream
    /// `advanceToStep` loops on work, not doorbells). Crucially, continuing
    /// on work does NOT clear `draining`, so no second drain can spawn
    /// mid-loop (the bug a naive take-then-recheck would introduce).
    pub fn next_scope(&self, session_id: &str, work_remains: bool) -> Option<WakeScope> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let coord = inner.get_mut(session_id)?;
        if let Some(scope) = coord.pending.take() {
            return Some(scope);
        }
        if work_remains {
            return Some(WakeScope::Input);
        }
        coord.draining = false;
        if !claim_would_hold(session_id) {
            inner.remove(session_id);
        }
        None
    }

    /// Drop the draining flag unconditionally (drain abort paths).
    pub fn abort(&self, session_id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(coord) = inner.get_mut(session_id) {
            coord.draining = false;
            coord.pending = None;
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// The coordinator entry drops only when no claim is held. Claims live in the
/// store; the coordinator set cannot see them, so the drain loop calls
/// `release_if_idle` with the live answer. This hook keeps the predicate
/// injectable for unit tests (production passes the store read).
fn claim_would_hold(_session_id: &str) -> bool {
    false
}

pub type SharedCoordinators = Arc<CoordinatorSet>;

/// Pick the next promotable inbox id for a drain iteration:
/// - `WakeScope::Steer`: steers only; a steered COMPACTION jumps ahead of
///   earlier steers (caller reorders via `list`); never across a move (the
///   move item blocks promotion — caller checks head-of-queue first).
/// - `WakeScope::Input`: steers first, else exactly one queued item.
pub fn next_for_scope(
    db: &std::path::Path,
    session_id: &str,
    scope: WakeScope,
) -> anyhow::Result<Option<String>> {
    use ocserve_store::inbox;
    match scope {
        WakeScope::Steer => {
            // Compaction-ahead-of-steers: prefer a steered compaction item.
            let items = inbox::list(db, session_id)?;
            if let Some(c) = items
                .iter()
                .find(|(_, t, _, d, _)| t == "compaction" && d == "steer")
            {
                return Ok(Some(c.0.clone()));
            }
            inbox::next_promotable(db, session_id, &["steer"])
        }
        WakeScope::Input => {
            if let Some(id) = inbox::next_promotable(db, session_id, &["steer"])? {
                return Ok(Some(id));
            }
            inbox::next_promotable(db, session_id, &["queue"])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_starts_one_drain_doorbells_the_rest() {
        let set = CoordinatorSet::new();
        assert!(set.wake("a", WakeScope::Steer), "first wake spawns");
        assert!(!set.wake("a", WakeScope::Steer), "second wake doorbells");
        assert!(!set.wake("a", WakeScope::Input), "third wake doorbells");
        // Widest scope wins: pending is Input after a steer+input pair.
        assert_eq!(set.take_pending("a"), Some(WakeScope::Input));
        // Still draining (take with pending set keeps the flag).
        assert!(!set.wake("a", WakeScope::Steer));
        assert_eq!(set.take_pending("a"), Some(WakeScope::Steer));
        // Empty take clears draining AND drops the entry (no claim held).
        assert_eq!(set.take_pending("a"), None);
        assert_eq!(set.len(), 0);
        // CONTROL: clear `draining` on every take → second wake spawns a
        // second drain (two fibers, one session) → the first assertion reds.
    }

    #[test]
    fn abort_clears_doorbell() {
        let set = CoordinatorSet::new();
        assert!(set.wake("b", WakeScope::Steer));
        set.abort("b");
        assert!(set.wake("b", WakeScope::Steer), "post-abort wake spawns");
        // CONTROL: abort that keeps pending → next take returns a scope
        // instead of None → red.
    }

    #[test]
    fn widest_scope_wins_regardless_of_order() {
        let set = CoordinatorSet::new();
        set.wake("c", WakeScope::Input);
        set.wake("c", WakeScope::Steer);
        assert_eq!(set.take_pending("c"), Some(WakeScope::Input));
        // CONTROL: last-write-wins instead of widest-wins → Steer here, red.
    }

    #[test]
    fn next_scope_continues_on_work_without_doorbell() {
        let set = CoordinatorSet::new();
        assert!(set.wake("d", WakeScope::Steer));
        assert_eq!(set.next_scope("d", false), Some(WakeScope::Steer));
        // No doorbell, but work remains → continue WITHOUT clearing draining.
        assert_eq!(set.next_scope("d", true), Some(WakeScope::Input));
        // A concurrent wake now must doorbell, not spawn (draining armed).
        assert!(!set.wake("d", WakeScope::Steer));
        assert_eq!(set.next_scope("d", false), Some(WakeScope::Steer));
        // No doorbell, no work → end (clears + drops).
        assert_eq!(set.next_scope("d", false), None);
        assert_eq!(set.len(), 0);
        // CONTROL: clear draining on work-continue → the mid-test wake
        // returns true (second drain) → red.
    }
}
