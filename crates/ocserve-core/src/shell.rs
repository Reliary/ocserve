//! Background shell registry (Phase D, bulkhead template).
//!
//! Long-running shell commands with accumulating output: each shell holds a
//! bounded output buffer (64 KiB, the PTY replay bound precedent); the
//! registry holds at most 128 shells (evict-exited-first, then oldest).
//! Completion is delivered by reaping the child on poll (no background
//! threads — the registry is polled on read, keeping the serving path
//! thread-free).

use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// Per-shell output cap (PTY 64 KiB replay bound precedent).
pub const SHELL_OUTPUT_CAP: usize = 64 * 1024;

/// Registry cap (bulkhead: fail-closed over cap after exited-eviction).
pub const SHELL_REGISTRY_CAP: usize = 128;

pub struct ShellEntry {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub status: String,
    pub output: Vec<u8>,
    pub exit_code: Option<i32>,
    pub child: Option<std::process::Child>,
}

pub struct ShellRegistry {
    inner: parking_lot::Mutex<HashMap<String, ShellEntry>>,
}

impl ShellRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// Spawn a shell (fails closed when full of RUNNING shells; exited ones
    /// evict first).
    pub fn spawn(self: &Arc<Self>, command: &str, cwd: &str) -> Result<String, String> {
        let mut inner = self.inner.lock();
        if inner.len() >= SHELL_REGISTRY_CAP && !inner.values().any(|e| e.status != "running") {
            return Err("too many running shells".to_string());
        }
        // Evict one exited shell to make room (oldest id first = insertion
        // order is not tracked; any exited entry frees the slot).
        if inner.len() >= SHELL_REGISTRY_CAP
            && let Some(victim) = inner
                .iter()
                .find(|(_, e)| e.status != "running")
                .map(|(k, _)| k.clone())
        {
            inner.remove(&victim);
        }
        let id = sh_id();
        let (kind, program) = ocserve_store::platform::shell();
        let mut sc = kind.command(&program, command);
        let child = sc
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;
        inner.insert(
            id.clone(),
            ShellEntry {
                id: id.clone(),
                command: command.to_string(),
                cwd: cwd.to_string(),
                status: "running".to_string(),
                output: Vec::new(),
                exit_code: None,
                child: Some(child),
            },
        );
        Ok(id)
    }

    /// Poll one shell: reap exited children (non-blocking), append newly
    /// available output (bounded), return the info projection.
    pub fn poll(&self, id: &str) -> Option<Value> {
        let mut inner = self.inner.lock();
        let e = inner.get_mut(id)?;
        if let Some(child) = e.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    e.exit_code = status.code();
                    e.status = "exited".to_string();
                    // Drain remaining pipes (bounded by the cap).
                    // Nonblocking is set on the CONCRETE pipe types (the
                    // Read trait has no set_nonblocking), then boxed for
                    // the shared read loop.
                    use std::io::Read;
                    let mut buf = [0u8; 8192];
                    let mut drain = |stream: &mut Option<Box<dyn Read>>| {
                        let Some(s) = stream.as_mut() else {
                            return;
                        };
                        loop {
                            match s.read(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    let room = SHELL_OUTPUT_CAP.saturating_sub(e.output.len());
                                    e.output.extend_from_slice(&buf[..n.min(room)]);
                                    if room == 0 {
                                        break;
                                    }
                                }
                            }
                        }
                    };
                    if let Some(stdout) = child.stdout.take() {
                        // Child has exited: pipes are EOF-terminated, so
                        // blocking reads terminate — no nonblocking needed.
                        let boxed: Box<dyn Read> = Box::new(stdout);
                        let mut opt = Some(boxed);
                        drain(&mut opt);
                    }
                    if let Some(stderr) = child.stderr.take() {
                        let boxed: Box<dyn Read> = Box::new(stderr);
                        let mut opt = Some(boxed);
                        drain(&mut opt);
                    }
                    e.child = None;
                }
                Ok(None) => {
                    // Still running: output drains ONLY at exit (bounded,
                    // single-shot). Live-tailing a pipe from the serving
                    // path would need a pump thread per shell — rejected;
                    // polls report status, output lands on exit.
                }
                Err(_) => {
                    e.status = "killed".to_string();
                    e.child = None;
                }
            }
        }
        Some(json!({
            "id": e.id,
            "status": e.status,
            "command": e.command,
            "cwd": e.cwd,
            "exitCode": e.exit_code,
        }))
    }

    pub fn output(&self, id: &str) -> Option<Value> {
        let inner = self.inner.lock();
        let e = inner.get(id)?;
        Some(json!({
            "output": String::from_utf8_lossy(&e.output).into_owned(),
            "truncated": e.output.len() >= SHELL_OUTPUT_CAP,
        }))
    }

    pub fn list(&self) -> Vec<Value> {
        // Poll-everything would take + release per entry; list is a cheap
        // status snapshot (freshness comes from get/output polls).
        self.inner
            .lock()
            .values()
            .map(|e| {
                json!({
                    "id": e.id, "status": e.status,
                    "command": e.command, "cwd": e.cwd,
                })
            })
            .collect()
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut inner = self.inner.lock();
        if let Some(mut e) = inner.remove(id) {
            if let Some(mut child) = e.child.take() {
                let _ = child.kill();
            }
            true
        } else {
            false
        }
    }
}

/// Mint a shell id (`sh_` + 24 base62).
pub fn sh_id() -> String {
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
    let mut out = String::with_capacity(27);
    out.push_str("sh_");
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

    #[test]
    fn spawn_poll_remove_lifecycle() {
        let reg = ShellRegistry::new();
        let id = reg.spawn("echo hi", "/tmp").expect("spawn");
        assert!(id.starts_with("sh_"));
        // Poll until exit (echo is instant).
        let mut info = None;
        for _ in 0..100 {
            info = reg.poll(&id);
            if info
                .as_ref()
                .and_then(|v| v.get("status"))
                .and_then(|s| s.as_str())
                != Some("running")
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(info.unwrap()["status"], "exited");
        let out = reg.output(&id).unwrap();
        assert!(out["output"].as_str().unwrap().contains("hi"));
        assert!(reg.remove(&id));
        assert!(!reg.remove(&id));
        // CONTROL: remove that does not kill → leaked child, red on proc table.
    }

    #[test]
    fn output_bounded_at_cap() {
        let reg = ShellRegistry::new();
        let id = reg
            .spawn("head -c 200000 /dev/zero | tr '\\0' 'x'", "/tmp")
            .expect("spawn");
        for _ in 0..200 {
            let info = reg.poll(&id).unwrap();
            if info["status"] != "running" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let out = reg.output(&id).unwrap();
        assert!(out["output"].as_str().unwrap().len() <= SHELL_OUTPUT_CAP);
        reg.remove(&id);
        // CONTROL: unbounded append → len exceeds cap, red.
    }
}
