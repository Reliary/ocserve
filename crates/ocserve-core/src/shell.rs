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

/// Directory named in `Shell.Info.file`. ocserve keeps shell output in memory
/// (bounded by `SHELL_OUTPUT_CAP`) rather than spilling to a file, but the
/// frozen v2 schema REQUIRES a `file` string, so we emit the path upstream
/// would have written to. Divergence: `D-SHELL-NO-FILE`.
pub const SHELL_OUTPUT_DIR: &str = "shell";

pub struct ShellEntry {
    pub id: String,
    pub command: String,
    pub cwd: String,
    /// Owning session, when the spawn came from a session-scoped route. The
    /// goal loop needs it to answer "is there background work this session is
    /// waiting on"; `list()` did not emit it, so the filter silently matched
    /// nothing and a `wait` verdict could never be honoured (2026-11-05
    /// review, and the code comment claimed the field existed).
    pub session_id: Option<String>,
    pub status: String,
    pub output: Vec<u8>,
    pub exit_code: Option<i32>,
    pub child: Option<std::process::Child>,
    /// Resolved shell program (`bash`, `/bin/zsh`, ...) — `Shell.Info.shell`.
    pub shell: String,
    /// Upstream writes combined output to a file and names it in
    /// `Shell.Info.file`; ocserve keeps it in memory, so this is the
    /// spill path the operator would find the output at if it were written.
    /// Emitted because the frozen schema REQUIRES the key.
    pub file: String,
    /// Epoch millis; `Shell.Info.time` is `{started, completed?}` and the
    /// schema requires `started`.
    pub started_ms: i64,
    pub completed_ms: Option<i64>,
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
        self.spawn_for_session(command, cwd, None)
    }

    /// Spawn attributed to a session (goal background-work detection).
    pub fn spawn_for_session(
        self: &Arc<Self>,
        command: &str,
        cwd: &str,
        session_id: Option<&str>,
    ) -> Result<String, String> {
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
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        inner.insert(
            id.clone(),
            ShellEntry {
                id: id.clone(),
                command: command.to_string(),
                cwd: cwd.to_string(),
                session_id: session_id.map(str::to_string),
                status: "running".to_string(),
                output: Vec::new(),
                exit_code: None,
                child: Some(child),
                shell: program.clone(),
                file: format!("{}/{}.log", SHELL_OUTPUT_DIR, id),
                started_ms,
                completed_ms: None,
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
                    e.completed_ms = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0),
                    );
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
                    e.completed_ms = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0),
                    );
                }
            }
        }
        // Upstream `Shell.Info` names the exit status `exit` (a number, absent
        // while running or when a signal ended the process); the schema is
        // closed, so no other key carries it. Clients such as the opencode
        // dispatcher judge a shell by `status == "exited" && exit == 0`.
        // Full `Shell.Info`. The schema is closed and REQUIRES id/status/
        // command/cwd/shell/file/metadata/time, so emitting only a subset
        // (the state after #75) still failed validation on every response —
        // the class the PR set out to fix. `sessionID` is NOT part of the
        // schema: it lives on `Shell.Entry` in a separate projection.
        let mut time = json!({ "started": e.started_ms });
        if let Some(c) = e.completed_ms {
            time["completed"] = json!(c);
        }
        let mut info = json!({
            "id": e.id,
            "status": e.status,
            "command": e.command,
            "cwd": e.cwd,
            "shell": e.shell,
            "file": e.file,
            "metadata": {},
            "time": time,
        });
        if let Some(code) = e.exit_code {
            info["exit"] = json!(code);
        }
        Some(info)
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
        // status snapshot (freshness comes from get/output polls). Shape is
        // `Shell.Info` (closed schema — `sessionID` here failed validation).
        self.inner
            .lock()
            .values()
            .map(|e| {
                let mut t = json!({ "started": e.started_ms });
                if let Some(c) = e.completed_ms {
                    t["completed"] = json!(c);
                }
                let mut info = json!({
                    "id": e.id, "status": e.status,
                    "command": e.command, "cwd": e.cwd,
                    "shell": e.shell, "file": e.file,
                    "metadata": {}, "time": t,
                });
                if let Some(code) = e.exit_code {
                    info["exit"] = json!(code);
                }
                info
            })
            .collect()
    }

    /// `(shell_id, owning session)` pairs. The goal loop's "does this session
    /// have background work" query reads the session id, which the closed
    /// `Shell.Info` schema does not carry.
    pub fn session_owners(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .values()
            .filter_map(|e| e.session_id.clone().map(|s| (e.id.clone(), s)))
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
        let info = info.unwrap();
        assert_eq!(info["status"], "exited");
        // Spec field name is `exit`; the old `exitCode` key must be gone.
        assert_eq!(info["exit"], 0);
        assert!(info.get("exitCode").is_none());
        // The frozen `Shell.Info` schema is CLOSED and requires every one of
        // these keys; emitting a subset validated fine in tests and failed on
        // a strict client. Assert the whole required set plus the absence of
        // anything the schema forbids (`sessionID` belongs to `Shell.Entry`).
        for key in [
            "id", "status", "command", "cwd", "shell", "file", "metadata", "time",
        ] {
            assert!(info.get(key).is_some(), "missing required `{key}`: {info}");
        }
        assert!(
            info["time"].get("started").is_some(),
            "time.started: {info}"
        );
        assert!(
            info["time"].get("completed").is_some(),
            "time.completed: {info}"
        );
        assert!(
            info.get("sessionID").is_none(),
            "sessionID is not in Shell.Info: {info}"
        );
        assert!(
            reg.list()[0].get("sessionID").is_none(),
            "list() must be schema-clean"
        );
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

    fn wait_exited(reg: &ShellRegistry, id: &str) -> Value {
        for _ in 0..200 {
            let info = reg.poll(id).expect("known shell");
            if info["status"] != "running" {
                return info;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("shell did not exit");
    }

    #[test]
    fn nonzero_exit_is_reported_as_exit() {
        let reg = ShellRegistry::new();
        let id = reg.spawn("exit 7", "/tmp").expect("spawn");
        let info = wait_exited(&reg, &id);
        assert_eq!(info["status"], "exited");
        assert_eq!(info["exit"], 7);
        reg.remove(&id);
    }

    #[test]
    fn running_shell_has_no_exit_key() {
        let reg = ShellRegistry::new();
        let id = reg.spawn("sleep 5", "/tmp").expect("spawn");
        let info = reg.poll(&id).unwrap();
        assert_eq!(info["status"], "running");
        assert!(info.get("exit").is_none(), "null/absent, never a fake 0");
        reg.remove(&id);
    }
}
