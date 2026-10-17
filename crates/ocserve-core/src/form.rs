//! Form rendezvous — v2 `Form` service (V2-PLAN.md P4, L3-lite).
//!
//! Mirrors `QuestionGate` (pending map + oneshot + Drop guard + bounded
//! wait) with v2 differences: ids are `^frm_`, the answer is a JSON object
//! (not label arrays), cancel is explicit (not a reject twin), and state
//! persists in the `form` table so reconnect replays settled forms.
//! Settled results: `Answered(Value)` / `Cancelled` / timeout→Cancelled.
//! Bounded: 512 pending forms max (permission-ask cap precedent); waiter
//! cap replaces upstream's infinite await (AGENTS §2.3).

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Runner-side bound for an unanswered form (upstream waits forever).
pub const FORM_WAIT: Duration = Duration::from_secs(1800);

/// Pending-form cap (permission-gate precedent: 512 persistent asks).
pub const FORM_CAP: usize = 512;

#[derive(Debug, Clone)]
pub enum FormOutcome {
    /// reply: the answer object.
    Answered(Value),
    /// cancel / wait timeout / channel closed.
    Cancelled,
}

struct Pending {
    request: Value,
    tx: tokio::sync::oneshot::Sender<FormOutcome>,
}

/// Pending form requests across sessions.
pub struct FormGate {
    pending: parking_lot::Mutex<HashMap<String, Pending>>,
    /// Abort-cleanup guards, one per pending form. Held here (not leaked):
    /// reply/cancel/wait-timeout remove the guard alongside the waiter, and
    /// dropping the gate (runner abort) removes the waiter via the guard.
    /// Without this the guard would have to be mem::forgotten (leak) or the
    /// abort path would leave a phantom waiter.
    guards: parking_lot::Mutex<HashMap<String, FormGuard>>,
}

/// Dropping the runner's guard removes its pending entry (abort of the
/// background prompt task also settles the DB row via the caller).
pub struct FormGuard {
    gate: Arc<FormGate>,
    id: String,
}

impl Drop for FormGuard {
    fn drop(&mut self) {
        self.gate.pending.lock().remove(&self.id);
    }
}

impl FormGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: parking_lot::Mutex::new(HashMap::new()),
            guards: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// Register a pending ask; returns the outcome receiver. The abort guard
    /// is held INSIDE the gate and released on settle (reply/cancel/timeout).
    /// Over-cap registers fail closed (None).
    pub fn register(
        self: &Arc<Self>,
        id: &str,
        request: Value,
    ) -> Option<tokio::sync::oneshot::Receiver<FormOutcome>> {
        let mut pending = self.pending.lock();
        if pending.len() >= FORM_CAP && !pending.contains_key(id) {
            return None;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        pending.insert(id.to_string(), Pending { request, tx });
        self.guards.lock().insert(
            id.to_string(),
            FormGuard {
                gate: Arc::clone(self),
                id: id.to_string(),
            },
        );
        Some(rx)
    }

    /// Reply: deliver the answer object. Unknown id → false.
    pub fn reply(&self, id: &str, answer: Value) -> bool {
        // Entry-existence verdict (send best-effort — see attempt.rs).
        let existed = match self.pending.lock().remove(id) {
            Some(p) => {
                let _ = p.tx.send(FormOutcome::Answered(answer));
                true
            }
            None => false,
        };
        self.guards.lock().remove(id);
        existed
    }

    /// Cancel: settle as cancelled. Unknown id → false.
    pub fn cancel(&self, id: &str) -> bool {
        let existed = match self.pending.lock().remove(id) {
            Some(p) => {
                let _ = p.tx.send(FormOutcome::Cancelled);
                true
            }
            None => false,
        };
        self.guards.lock().remove(id);
        existed
    }

    /// Runner wait with the bounded cap; timeout → Cancelled + cleanup.
    pub async fn wait(
        &self,
        id: &str,
        rx: tokio::sync::oneshot::Receiver<FormOutcome>,
    ) -> FormOutcome {
        let out = match tokio::time::timeout(FORM_WAIT, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => FormOutcome::Cancelled,
            Err(_) => {
                self.pending.lock().remove(id);
                FormOutcome::Cancelled
            }
        };
        self.guards.lock().remove(id);
        out
    }

    /// All pending requests (global `/api/form` list).
    pub fn list(&self) -> Vec<Value> {
        self.pending
            .lock()
            .values()
            .map(|p| p.request.clone())
            .collect()
    }
}

/// Mint a form id (`frm_` + 24 base62 — the frozen `^frm_` brand).
pub fn frm_id() -> String {
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
    out.push_str("frm_");
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
    use serde_json::json;

    #[tokio::test]
    async fn reply_delivers_answer_cancel_settles() {
        let gate = FormGate::new();
        let rx = gate
            .register("frm_1", json!({"id": "frm_1"}))
            .expect("under cap");
        assert!(gate.reply("frm_1", json!({"a": 1})));
        match gate.wait("frm_1", rx).await {
            FormOutcome::Answered(v) => assert_eq!(v, json!({"a": 1})),
            other => panic!("wrong outcome: {other:?}"),
        }
        assert!(
            !gate.reply("frm_1", json!(null)),
            "settled forms stay settled"
        );
        let rx2 = gate.register("frm_2", json!({"id": "frm_2"})).unwrap();
        assert!(gate.cancel("frm_2"));
        assert!(matches!(
            gate.wait("frm_2", rx2).await,
            FormOutcome::Cancelled
        ));
        // CONTROL: reply that does not remove → second reply succeeds, red.
    }

    #[test]
    fn frm_id_brand_and_uniqueness() {
        let a = frm_id();
        let b = frm_id();
        assert!(a.starts_with("frm_") && a.len() == 28);
        assert_ne!(a, b);
    }
}
