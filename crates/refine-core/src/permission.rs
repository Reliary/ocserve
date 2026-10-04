//! Permission rendezvous: ask → permission.asked event → await reply via
//! POST /permission/{id}/reply (oc-remote contract: reply = once|always|reject).
//! Bounded wait (300s) → deny (fail-closed, never hang a session).

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// How long an unanswered ask blocks the tool (then deny).
pub const ASK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Default)]
pub struct PermissionGate {
    pending: parking_lot::Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>,
    /// session → "<permission>:<resource>" keys granted "always" this session
    always: parking_lot::Mutex<HashMap<String, Vec<String>>>,
    /// sessions whose persisted grants were loaded (one DB read per prompt,
    /// not per ask — K-ALWAYS persistence)
    hydrated: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// snapshots for GET /permission
    requests: parking_lot::Mutex<HashMap<String, Value>>,
}

/// Drop-guard: mirrors question::PendingGuard (happy-path-only cleanup is a
/// banned class — AGENTS §2.3/§2.5; antagonism A2).
pub struct PendingGuard {
    gate: Arc<PermissionGate>,
    id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.gate.pending.lock().remove(&self.id);
        self.gate.requests.lock().remove(&self.id);
    }
}

impl PermissionGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a pending ask; returns the receiver AND a drop-guard. The
    /// guard removes both pending+requests entries on ANY exit (abort/panic
    /// included) — timeout cleanup alone misses the killed-task case
    /// (antagonism A2; QuestionGate's PendingGuard is the same pattern).
    pub fn register(
        self: Arc<Self>,
        id: &str,
        request: Value,
    ) -> (tokio::sync::oneshot::Receiver<String>, PendingGuard) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().insert(id.to_string(), tx);
        self.requests.lock().insert(id.to_string(), request);
        let guard = PendingGuard {
            gate: self,
            id: id.to_string(),
        };
        (rx, guard)
    }

    /// K-ALWAYS: load the session's persisted always-grants into memory
    /// (once per session per process; prompt lock serializes prompts so the
    /// read-then-insert cannot race itself).
    pub fn hydrate(&self, db: &std::path::Path, session_id: &str) -> anyhow::Result<()> {
        if self.hydrated.lock().contains(session_id) {
            return Ok(());
        }
        let keys = refine_store::session_always_keys(db, session_id)?;
        if !keys.is_empty() {
            let mut always = self.always.lock();
            let entry = always.entry(session_id.to_string()).or_default();
            for k in keys {
                if !entry.contains(&k) {
                    entry.push(k);
                }
            }
        }
        self.hydrated.lock().insert(session_id.to_string());
        Ok(())
    }

    /// Gauge support: number of asks awaiting a reply.
    pub fn pending_len(&self) -> usize {
        self.pending.lock().len()
    }

    /// Resolve a pending ask from a client reply. Unknown id → false.
    pub fn reply(&self, id: &str, reply: &str) -> bool {
        self.requests.lock().remove(id);
        match self.pending.lock().remove(id) {
            Some(tx) => tx.send(reply.to_string()).is_ok(),
            None => false,
        }
    }

    /// Await the reply with timeout; timeout → "reject" (fail closed) AND
    /// cleanup (a stale pending entry would 404 every later reply — M2b finding).
    pub async fn wait(&self, id: &str, rx: tokio::sync::oneshot::Receiver<String>) -> String {
        match tokio::time::timeout(ASK_TIMEOUT, rx).await {
            Ok(Ok(reply)) => reply,
            _ => {
                self.requests.lock().remove(id);
                self.pending.lock().remove(id);
                "reject".to_string()
            }
        }
    }

    /// "always" grant: remember key for the session (in-memory, M2b scope).
    pub fn grant_always(&self, session_id: &str, key: &str) {
        self.always
            .lock()
            .entry(session_id.to_string())
            .or_default()
            .push(key.to_string());
    }

    pub fn check_always(&self, session_id: &str, key: &str) -> bool {
        self.always
            .lock()
            .get(session_id)
            .map(|v| v.iter().any(|k| k == key))
            .unwrap_or(false)
    }

    /// Pending request list for GET /permission.
    pub fn list(&self) -> Vec<Value> {
        self.requests.lock().values().cloned().collect()
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    #[test]
    fn dropping_pending_guard_clears_entries() {
        let gate = Arc::new(PermissionGate::default());
        let (rx, guard) = gate
            .clone()
            .register("perm_t", serde_json::json!({"tool":"bash"}));
        assert_eq!(gate.pending_len(), 1);
        drop(guard); // abort path: the ask future dies without reply/timeout
        assert_eq!(gate.pending_len(), 0, "guard must clear pending (A2)");
        assert!(!gate.reply("perm_t", "once"), "already cleared");
        drop(rx);
    }

    #[test]
    fn reply_then_guard_drop_is_idempotent() {
        let gate = Arc::new(PermissionGate::default());
        let (rx, guard) = gate.clone().register("perm_r", serde_json::json!({}));
        assert!(gate.reply("perm_r", "allow"));
        assert_eq!(gate.pending_len(), 0);
        drop(guard); // no-op after reply
        assert_eq!(gate.pending_len(), 0);
        drop(rx);
    }
}
