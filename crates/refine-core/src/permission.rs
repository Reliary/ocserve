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
    /// snapshots for GET /permission
    requests: parking_lot::Mutex<HashMap<String, Value>>,
}

impl PermissionGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a pending ask; returns the receiver for the reply string.
    pub fn register(&self, id: &str, request: Value) -> tokio::sync::oneshot::Receiver<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().insert(id.to_string(), tx);
        self.requests.lock().insert(id.to_string(), request);
        rx
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
