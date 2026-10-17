//! Integration-connect attempt rendezvous (Phase D, bulkhead template).
//!
//! Mirrors `FormGate` (pending map + oneshot + Drop guard + bounded wait +
//! cap): attempts are ephemeral connect flows (command/external/oauth) that
//! resolve via complete/cancel or timeout. Over-cap registers fail closed.
//! Attempts are IN-MEMORY (upstream attempts don't survive restarts either —
//! they gate live OAuth/command handshakes, not durable state).

use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Runner-side bound for an unresolved attempt.
pub const ATTEMPT_WAIT: Duration = Duration::from_secs(600);

/// Pending-attempt cap (rendezvous template: 512, fail-closed).
pub const ATTEMPT_CAP: usize = 512;

#[derive(Debug, Clone)]
pub enum AttemptOutcome {
    Completed(Value),
    Cancelled,
}

struct Pending {
    request: Value,
    tx: tokio::sync::oneshot::Sender<AttemptOutcome>,
}

/// Pending connect attempts across integrations.
pub struct AttemptGate {
    pending: parking_lot::Mutex<HashMap<String, Pending>>,
    guards: parking_lot::Mutex<HashMap<String, AttemptGuard>>,
}

/// Dropping the guard removes its pending entry (abort cleanup).
pub struct AttemptGuard {
    gate: Arc<AttemptGate>,
    id: String,
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        self.gate.pending.lock().remove(&self.id);
    }
}

impl AttemptGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: parking_lot::Mutex::new(HashMap::new()),
            guards: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// Register an attempt; the abort guard lives inside the gate.
    pub fn register(
        self: &Arc<Self>,
        id: &str,
        request: Value,
    ) -> Option<tokio::sync::oneshot::Receiver<AttemptOutcome>> {
        let mut pending = self.pending.lock();
        if pending.len() >= ATTEMPT_CAP && !pending.contains_key(id) {
            return None;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        pending.insert(id.to_string(), Pending { request, tx });
        self.guards.lock().insert(
            id.to_string(),
            AttemptGuard {
                gate: Arc::clone(self),
                id: id.to_string(),
            },
        );
        Some(rx)
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        self.pending.lock().get(id).map(|p| p.request.clone())
    }

    pub fn complete(&self, id: &str, answer: Value) -> bool {
        // Entry-existence is the verdict (send is best-effort: fire-and-
        // forget creators drop the receiver, and a dropped rx must NOT turn
        // a real settlement into a 404 — proven by the phase-D e2e).
        let existed = match self.pending.lock().remove(id) {
            Some(p) => {
                let _ = p.tx.send(AttemptOutcome::Completed(answer));
                true
            }
            None => false,
        };
        self.guards.lock().remove(id);
        existed
    }

    pub fn cancel(&self, id: &str) -> bool {
        let existed = match self.pending.lock().remove(id) {
            Some(p) => {
                let _ = p.tx.send(AttemptOutcome::Cancelled);
                true
            }
            None => false,
        };
        self.guards.lock().remove(id);
        existed
    }

    pub async fn wait(
        &self,
        id: &str,
        rx: tokio::sync::oneshot::Receiver<AttemptOutcome>,
    ) -> AttemptOutcome {
        let out = match tokio::time::timeout(ATTEMPT_WAIT, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => AttemptOutcome::Cancelled,
            Err(_) => {
                self.pending.lock().remove(id);
                AttemptOutcome::Cancelled
            }
        };
        self.guards.lock().remove(id);
        out
    }
}

/// Mint an attempt id (`att_` + 24 base62).
pub fn att_id() -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(nanos.to_be_bytes());
    h.update(std::process::id().to_be_bytes());
    let digest = h.finalize();
    const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut out = String::with_capacity(28);
    out.push_str("att_");
    let mut acc: u128 = 0;
    for &byte in digest.iter().take(16) {
        acc = (acc << 8) | byte as u128;
    }
    for _ in 0..24 {
        out.push(B62[(acc % 62) as usize] as char);
        acc /= 62;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn complete_delivers_cancel_settles() {
        let gate = AttemptGate::new();
        let rx = gate
            .register("att_1", json!({"id": "att_1"}))
            .expect("under cap");
        assert!(gate.complete("att_1", json!({"ok": true})));
        match gate.wait("att_1", rx).await {
            AttemptOutcome::Completed(v) => assert_eq!(v, json!({"ok": true})),
            other => panic!("wrong outcome: {other:?}"),
        }
        assert!(
            !gate.complete("att_1", json!(null)),
            "settled stays settled"
        );
        // CONTROL: complete that does not remove → second complete succeeds, red.
    }

    #[test]
    fn complete_without_waiter_still_settles() {
        // Fire-and-forget creators drop the receiver: settlement verdict is
        // entry-existence, not send-ok (the phase-D e2e caught send-ok
        // turning real settlements into 404s).
        let gate = AttemptGate::new();
        let _rx = gate
            .register("att_2", json!({"id": "att_2"}))
            .expect("under cap");
        drop(_rx);
        assert!(gate.complete("att_2", json!({"ok": true})));
        assert!(!gate.complete("att_2", json!(null)));
        // CONTROL: send-ok verdict → first assert fails (rx dropped), red.
    }
}
