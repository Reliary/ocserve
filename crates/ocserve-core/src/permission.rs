//! Permission rendezvous: ask → permission.asked event → await reply via
//! POST /permission/{id}/reply (oc-remote contract: reply = once|always|reject).
//! Bounded wait (300s) → deny (fail-closed, never hang a session).

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// How long an unanswered ask blocks the tool (then deny).
pub const ASK_TIMEOUT: Duration = Duration::from_secs(300);

/// An "always"-granted rule (upstream `Permission.Rule`): a wildcard pattern
/// per permission family. A grant on `bash` with pattern `git status *` covers
/// every future `git status …` — unlike ocserve's old exact-string keys.
#[derive(Clone, Debug, PartialEq)]
pub struct GrantRule {
    pub permission: String,
    pub pattern: String,
}

#[derive(Default)]
pub struct PermissionGate {
    pending: parking_lot::Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>,
    /// session → granted rules ("always" replies), evaluated with wildcards
    always: parking_lot::Mutex<HashMap<String, Vec<GrantRule>>>,
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

    /// Register a pending ask that is NOT tied to a handler frame — the entry
    /// lives until `reply`/timeout (the v2 permission oracle's `ask`, whose
    /// request must survive the create handler returning). No waiter exists, so
    /// the receiver is dropped; `reply` keys on the entry's PRESENCE, not on a
    /// successful oneshot send, so this still resolves cleanly.
    pub fn register_persistent(&self, id: &str, request: Value) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().insert(id.to_string(), tx);
        self.requests.lock().insert(id.to_string(), request);
        drop(rx);
    }

    /// K-ALWAYS: load the session's persisted always-grants into memory
    /// (once per session per process; prompt lock serializes prompts so the
    /// read-then-insert cannot race itself).
    pub fn hydrate(&self, db: &std::path::Path, session_id: &str) -> anyhow::Result<()> {
        if self.hydrated.lock().contains(session_id) {
            return Ok(());
        }
        let keys = ocserve_store::session_always_keys(db, session_id)?;
        if !keys.is_empty() {
            let mut always = self.always.lock();
            let entry = always.entry(session_id.to_string()).or_default();
            for k in keys {
                // new form: "<permission>\t<pattern>"; legacy: "<permission>:<pattern>"
                let (permission, pattern) = match k.split_once('\t') {
                    Some((p, pat)) => (p.to_string(), pat.to_string()),
                    None => match k.split_once(':') {
                        Some((p, pat)) => (p.to_string(), pat.to_string()),
                        None => continue,
                    },
                };
                let rule = GrantRule {
                    permission,
                    pattern,
                };
                if !entry.contains(&rule) {
                    entry.push(rule);
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

    /// Resolve a pending ask from a client reply. Returns true when the id was
    /// pending (the request existed), regardless of whether a waiter was
    /// listening — the v2 oracle registers asks with no waiter.
    ///
    /// Cascade (upstream permission/index.ts `reply`): a `reject` fails every
    /// OTHER pending ask in the same session; an `always` auto-approves any
    /// pending ask whose patterns its new grants now cover. The reply string
    /// is forwarded verbatim so waiters see "once"/"always"/"reject".
    pub fn reply(&self, id: &str, reply: &str) -> bool {
        let request = self.requests.lock().remove(id);
        let Some(tx) = self.pending.lock().remove(id) else {
            return false;
        };
        let _ = tx.send(reply.to_string());
        let session_id = request
            .as_ref()
            .and_then(|r| r.get("sessionID"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        // Cascade only for the terminal replies.
        if reply == "reject"
            && let Some(sid) = &session_id
        {
            let others: Vec<String> = {
                let reqs = self.requests.lock();
                reqs.iter()
                    .filter(|(k, v)| {
                        *k != id
                            && v.get("sessionID").and_then(|x| x.as_str()) == Some(sid.as_str())
                    })
                    .map(|(k, _)| k.clone())
                    .collect()
            };
            for oid in others {
                self.requests.lock().remove(&oid);
                if let Some(otx) = self.pending.lock().remove(&oid) {
                    let _ = otx.send("reject".to_string());
                }
            }
        } else if reply == "always"
            && let Some(sid) = &session_id
        {
            // upstream reply pushes the request's `always` patterns as grants
            // BEFORE cascading, so the cascade (and future asks) see them.
            if let Some(req) = &request {
                let permission = req
                    .get("permission")
                    .or_else(|| req.get("action"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(always) = req.get("always").and_then(|v| v.as_array()) {
                    for pat in always.iter().filter_map(|p| p.as_str()) {
                        self.grant_always(sid, permission, pat);
                    }
                }
            }
            // re-check other pending asks against this session's grants and
            // auto-resolve any now covered (upstream's loop).
            let granted = self.granted(sid);
            let covered: Vec<String> = {
                let reqs = self.requests.lock();
                reqs.iter()
                    .filter(|(k, v)| {
                        *k != id
                            && v.get("sessionID").and_then(|x| x.as_str()) == Some(sid.as_str())
                            && patterns_covered(v, &granted)
                    })
                    .map(|(k, _)| k.clone())
                    .collect()
            };
            for oid in covered {
                self.requests.lock().remove(&oid);
                if let Some(otx) = self.pending.lock().remove(&oid) {
                    let _ = otx.send("always".to_string());
                }
            }
        }
        true
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

    /// "always" grant: remember a rule for the session. The store may pass the
    /// legacy `"<permission>:<pattern>"` or the new `"<permission>\t<pattern>"`
    /// form; both are parsed into a wildcard rule (upstream stores rules, not
    /// exact strings, so `git status *` covers `git status --short`).
    pub fn grant_always(&self, session_id: &str, permission: &str, pattern: &str) {
        let rule = GrantRule {
            permission: permission.to_string(),
            pattern: pattern.to_string(),
        };
        let mut always = self.always.lock();
        let entry = always.entry(session_id.to_string()).or_default();
        if !entry.contains(&rule) {
            entry.push(rule);
        }
    }

    /// Whether a granted rule covers this (permission, resource): the rule's
    /// pattern matches the resource with wildcards (upstream evaluate over
    /// `approved`).
    pub fn check_always(&self, session_id: &str, permission: &str, resource: &str) -> bool {
        self.always
            .lock()
            .get(session_id)
            .map(|rules| {
                rules.iter().any(|r| {
                    ocserve_tools::wildcard_match(permission, &r.permission)
                        && ocserve_tools::wildcard_match(resource, &r.pattern)
                })
            })
            .unwrap_or(false)
    }

    /// Snapshot of granted rules for a session (used by the reply cascade to
    /// re-evaluate sibling pending asks, upstream `reply` "always" branch).
    pub fn granted(&self, session_id: &str) -> Vec<GrantRule> {
        self.always
            .lock()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Pending request list for GET /permission.
    pub fn list(&self) -> Vec<Value> {
        self.requests.lock().values().cloned().collect()
    }
}

/// Whether every pattern of a pending request is covered by the granted rules
/// (upstream `item.info.patterns.every(evaluate(...) === "allow")`).
fn patterns_covered(request: &Value, granted: &[GrantRule]) -> bool {
    let permission = request
        .get("permission")
        .or_else(|| request.get("action"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let Some(patterns) = request.get("patterns").and_then(|v| v.as_array()) else {
        return false;
    };
    !patterns.is_empty()
        && patterns.iter().all(|p| {
            p.as_str().is_some_and(|pat| {
                granted.iter().any(|r| {
                    ocserve_tools::wildcard_match(permission, &r.permission)
                        && ocserve_tools::wildcard_match(pat, &r.pattern)
                })
            })
        })
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
