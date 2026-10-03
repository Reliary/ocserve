//! Sift at the provider boundary (DIFFERENTIATION.md §4 / P1c).
//!
//! Persist raw (client truth = upstream parity); compress ONLY when building
//! provider messages. Determinism is load-bearing: the live tool-result site
//! and the history-rebuild site must emit identical bytes or the provider
//! prefix cache busts on the next turn (our own cache law, PLAN_agent_integration).
//!
//! Default OFF (REFINE_SIFT unset/0/off). Enabling: `auto` (reliary on PATH,
//! ~/.local/bin prepended) or an explicit binary path (invoked as
//! `<path> sift --stdin`). Output rules, all hard invariants:
//! - whitelist: bash only (read/grep results are edited-from verbatim);
//! - >= 4096 bytes threshold;
//! - returned bytes MUST be non-empty (compress_unified can return "" on
//!   degenerate input — observed) and STRICTLY smaller than raw;
//! - marker line only when it still fits under raw (never inflate);
//! - subprocess timeout (2s) / spawn failure / missing binary => raw.
//!
//! Bounded memo cache (64 entries) so repeated history rebuilds do not
//! spawn per turn.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Arc, Mutex, OnceLock};

const MIN_BYTES: usize = 4096;
const TIMEOUT_MS: u64 = 2_000;
const CACHE_CAP: usize = 64;
const WHITELIST: &[&str] = &["bash"];

fn env_cmd() -> Option<Vec<String>> {
    let v = std::env::var("REFINE_SIFT").unwrap_or_default();
    match v.trim() {
        "" | "0" | "off" | "false" => None,
        "auto" => Some(vec!["reliary".into(), "sift".into(), "--stdin".into()]),
        path => Some(vec![path.into(), "sift".into(), "--stdin".into()]),
    }
}

/// per-command availability (test isolation: each fake path probes itself)
fn cmd_available(cmd: &[String]) -> bool {
    static AVAIL: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let map = AVAIL.get_or_init(|| Mutex::new(HashMap::new()));
    let key = cmd[0].clone();
    if let Some(v) = map.lock().expect("sift avail").get(&key) {
        return *v;
    }
    let ok = probe(cmd);
    map.lock().expect("sift avail").entry(key).or_insert(ok);
    ok
}

fn probe(cmd: &[String]) -> bool {
    let mut c = std::process::Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    // mirror the MCP PATH fix: systemd PATH lacks ~/.local/bin
    if let Ok(home) = std::env::var("HOME") {
        let old = std::env::var("PATH").unwrap_or_default();
        c.env("PATH", format!("{home}/.local/bin:{old}"));
    }
    // probe by re-exec with --help? no: trust spawn of the real stdin call
    // lazily instead — availability = the run itself; here we only verify
    // the binary resolves (spawn of `--help` is cheap and side-effect free)
    let _ = c.arg("--help");
    match c
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // bounded wait (probe only)
            for _ in 0..200 {
                match child.try_wait() {
                    Ok(Some(_)) => return true,
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
                    Err(_) => return false,
                }
            }
            let _ = child.kill();
            false
        }
        Err(_) => false,
    }
}

fn cache() -> &'static Mutex<HashMap<u64, Option<Arc<str>>>> {
    static C: OnceLock<Mutex<HashMap<u64, Option<Arc<str>>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn run_sift(cmd: &[String], raw: &str) -> Option<String> {
    let mut c = std::process::Command::new(&cmd[0]);
    c.args(&cmd[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Ok(home) = std::env::var("HOME") {
        let old = std::env::var("PATH").unwrap_or_default();
        c.env("PATH", format!("{home}/.local/bin:{old}"));
    }
    let mut child = c.spawn().ok()?;
    {
        let mut stdin = child.stdin.take()?;
        stdin.write_all(raw.as_bytes()).ok()?;
    } // EOF — reader thread drains stdout so a hung binary can't block us
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(TIMEOUT_MS);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok()?;
                if !status.success() {
                    return None;
                }
                return String::from_utf8(out).ok();
            }
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None; // timeout => caller falls back to raw
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

/// Process-global env lock for tests (REFINE_SIFT is shared across the
/// whole lib-test binary — every mutating test must hold this).
#[cfg(test)]
pub(crate) fn sift_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Provider-bound transform: returns the bytes to send for `output`.
pub fn maybe_sift(tool: &str, output: &str) -> Arc<str> {
    let raw_len = output.len();
    if raw_len < MIN_BYTES || !WHITELIST.contains(&tool) {
        return Arc::from(output);
    }
    let Some(cmd) = env_cmd() else {
        return Arc::from(output);
    };
    if !cmd_available(&cmd) {
        return Arc::from(output);
    }
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        tool.hash(&mut h);
        output.hash(&mut h);
        cmd[0].hash(&mut h);
        h.finish()
    };
    if let Some(hit) = cache().lock().expect("sift cache").get(&key) {
        return hit.clone().unwrap_or_else(|| Arc::from(output));
    }
    let result = match run_sift(&cmd, output) {
        // hard invariants: non-empty AND strictly smaller AND marker fits
        Some(new) if !new.is_empty() && new.len() < raw_len => {
            let marker = format!("[compressed {raw_len} -> {} bytes]\n", new.len());
            let combined: Arc<str> = if raw_len >= new.len() + marker.len() {
                Arc::from(format!("{marker}{new}"))
            } else {
                Arc::from(new)
            };
            if combined.len() <= raw_len {
                combined
            } else {
                Arc::from(output) // marker overflow — never inflate
            }
        }
        _ => Arc::from(output),
    };
    let sent_sifted = result.as_ref() != output;
    {
        let mut m = cache().lock().expect("sift cache");
        if m.len() >= CACHE_CAP {
            m.clear(); // bounded: cheap re-warm, no LRU bookkeeping
        }
        m.insert(
            key,
            if sent_sifted {
                Some(result.clone())
            } else {
                None
            },
        );
    }
    refine_metrics::labeled_counter(
        "refine_sift_total",
        if sent_sifted {
            "outcome=\"compressed\""
        } else {
            "outcome=\"raw\""
        },
        1,
    );
    refine_metrics::labeled_counter(
        "refine_sift_bytes_total",
        "direction=\"in\"",
        raw_len as u64,
    );
    refine_metrics::labeled_counter(
        "refine_sift_bytes_total",
        "direction=\"out\"",
        result.len() as u64,
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn big_noise() -> String {
        let mut s = String::new();
        for i in 0..600 {
            s.push_str(&format!("step {i}: compiling module m{i} ok\n"));
        }
        s
    }

    fn write_fake(name: &str, script: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("refine-sift-fake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, script).unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    fn set_sift(v: &str) {
        unsafe { std::env::set_var("REFINE_SIFT", v) }
    }

    /// shared lock (crate-level) — env is process-global across lib tests
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        super::sift_env_lock()
    }

    #[test]
    fn off_by_default_and_whitelist_and_threshold() {
        let _g = env_lock();
        unsafe { std::env::remove_var("REFINE_SIFT") };
        let raw = big_noise();
        assert_eq!(maybe_sift("bash", &raw).as_ref(), raw, "default off");
        set_sift("auto");
        // whitelist: read never sifts even when huge
        assert_eq!(maybe_sift("read", &raw).as_ref(), raw, "whitelist fence");
        // threshold: tiny output never spawns
        assert_eq!(maybe_sift("bash", "short").as_ref(), "short");
        unsafe { std::env::remove_var("REFINE_SIFT") };
    }

    #[test]
    fn fake_compressor_shrinks_with_marker_never_inflating() {
        let _g = env_lock();
        // hermetic fake: strips every line that starts with 'step' marker style
        // (emits a fixed short body regardless of input size)
        let fake = write_fake(
            "fake_shrink.sh",
            "#!/bin/sh\ncat >/dev/null\necho 'FAIL test_x: assertion failed'\necho 'ok-summary [400 ok]'\n",
        );
        set_sift(fake.to_str().unwrap());
        let raw = big_noise();
        let out = maybe_sift("bash", &raw);
        assert!(
            out.len() < raw.len(),
            "must shrink ({} < {})",
            out.len(),
            raw.len()
        );
        assert!(out.len() <= raw.len(), "never inflate");
        assert!(out.contains("[compressed"), "marker present: {out}");
        assert!(out.contains("assertion failed"), "signal preserved");
        // determinism: second call = cached identical bytes
        let out2 = maybe_sift("bash", &raw);
        assert_eq!(out.as_ref(), out2.as_ref(), "cache must be byte-identical");
        unsafe { std::env::remove_var("REFINE_SIFT") };
    }

    #[test]
    fn empty_result_falls_back_to_raw() {
        let _g = env_lock();
        // observed bug guard: compress_unified CAN return "" — must not blank
        let fake = write_fake("fake_empty.sh", "#!/bin/sh\ncat >/dev/null\n");
        set_sift(fake.to_str().unwrap());
        let raw = big_noise();
        let out = maybe_sift("bash", &raw);
        assert_eq!(
            out.as_ref(),
            raw,
            "empty compressed output must fall back to raw"
        );
        unsafe { std::env::remove_var("REFINE_SIFT") };
    }

    #[test]
    fn missing_binary_falls_back_to_raw() {
        let _g = env_lock();
        set_sift("/nonexistent/reliary-binary-xyz");
        let raw = big_noise();
        let out = maybe_sift("bash", &raw);
        assert_eq!(out.as_ref(), raw, "missing binary must fall back");
        unsafe { std::env::remove_var("REFINE_SIFT") };
    }

    #[test]
    fn inflation_falls_back_to_raw() {
        let _g = env_lock();
        // fake that OUTPUTS MORE than input (inflation attempt)
        let fake = write_fake(
            "fake_inflate.sh",
            "#!/bin/sh\ncat >/dev/null\nhead -c 999999 /dev/zero | tr '\\0' 'x'\n",
        );
        set_sift(fake.to_str().unwrap());
        let raw = big_noise();
        let out = maybe_sift("bash", &raw);
        assert_eq!(out.as_ref(), raw, "inflated output must fall back to raw");
        unsafe { std::env::remove_var("REFINE_SIFT") };
    }
}
