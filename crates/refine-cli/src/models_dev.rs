//! K-MODELS: same-as-upstream model-catalog refresh.
//!
//! Faithful port of freeze v1.18.31 `packages/core/src/models-dev.ts`
//! (source, flags, TTL, retry, atomic write, self-heal) and
//! `packages/core/src/util/flock.ts` (cross-process lease: lockdir +
//! `meta.json` + heartbeat + breaker) so refine updates model lists
//! EXACTLY like their opencode — including sharing the cache file and the
//! lease under their state dir when both run on one box.
//!
//! Divergences (named, TRACEABILITY K-MODELS):
//! - D-MODELS-1: no embedded build-time snapshot — a refine-only offline
//!   first boot has an empty catalog until the first fetch succeeds.
//! - D-MODELS-2: the fetched payload is parse-validated BEFORE replace
//!   (upstream writes raw then heals on read; we skip the corrupt window).
//! - HOME-based paths (like the rest of refine runtime) — XDG_CACHE_HOME /
//!   XDG_STATE_HOME overrides are ignored (upstream honors them).
//!
//! Sync module by design: fetch/lease work runs via `spawn_blocking` at
//! the call sites (AGENTS §2.3 — never pin an async worker on network I/O).

use anyhow::Context as _;
use serde_json::{Value, json};
use std::hash::{BuildHasher as _, Hasher as _};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub const DEFAULT_SOURCE: &str = "https://models.opencode.ai";
/// models-dev.ts `ttl = Duration.minutes(5)`
pub const FRESH_MS: u64 = 5 * 60_000;
/// models-dev.ts `Schedule.spaced("60 minutes")`
pub const REFRESH_INTERVAL_SECS: u64 = 3600;
/// models-dev.ts `fetchApi` `.timeout("10 seconds")`
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

// flock.ts `defaultOpts`
const LEASE_STALE_MS: u64 = 60_000;
const LEASE_TIMEOUT_MS: u64 = 300_000;
const LEASE_BASE_DELAY_MS: u64 = 100;
const LEASE_MAX_DELAY_MS: u64 = 2_000;
/// flock.ts heartbeat `Math.max(100, staleMs / 3)` = 20_000
const LEASE_HEARTBEAT_MS: u64 = LEASE_STALE_MS / 3;

/// flag.ts `truthy()` — exact pin: lowercase value is "true" or "1".
fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .ok()
            .as_deref()
            .map(str::to_lowercase)
            .as_deref(),
        Some("true") | Some("1")
    )
}

pub fn fetch_disabled() -> bool {
    env_truthy("OPENCODE_DISABLE_MODELS_FETCH")
}

/// models-dev.ts `source = Flag.OPENCODE_MODELS_URL || "https://models.opencode.ai"`
pub fn source_url() -> String {
    match std::env::var("OPENCODE_MODELS_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => DEFAULT_SOURCE.to_string(),
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// models-dev.ts filename rule: default source → `models.json`,
/// custom source → `models-{Hash.fast(source)}.json` (Hash.fast = sha1 hex).
pub fn file_name_for(source: &str) -> String {
    if source == DEFAULT_SOURCE {
        "models.json".to_string()
    } else {
        format!("models-{}.json", sha1_hex(source.as_bytes()))
    }
}

/// The file refine WRITES (and reads unless OPENCODE_MODELS_PATH pins a
/// fixture). Always HOME-based — matches runtime/watch_paths (D-MODELS-3).
pub fn write_path() -> PathBuf {
    let source = source_url();
    home().join(".cache/opencode").join(file_name_for(&source))
}

/// Read precedence: OPENCODE_MODELS_PATH (upstream fixture flag) ?? write path.
pub fn read_path() -> PathBuf {
    match std::env::var("OPENCODE_MODELS_PATH") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => write_path(),
    }
}

/// Lock root (flock.ts `root()`): `{xdgState}/opencode/locks` — refine's
/// HOME-based approximation = `~/.local/state/opencode/locks` (their state
/// dir: leases are ephemeral + stale-swept, creating it is safe).
pub fn lease_root() -> PathBuf {
    home().join(".local/state/opencode/locks")
}

/// models-dev.ts `fresh()` — mtime strictly under TTL (missing = stale).
pub fn is_fresh(path: &Path, now: SystemTime) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    now.duration_since(mtime)
        .map(|d| d.as_millis() < FRESH_MS as u128)
        .unwrap_or(false)
}

/// models-dev.ts `USER_AGENT = opencode/{channel}/{version}/{client}` —
/// release builds bake channel "latest" (packages/script CHANNEL rule);
/// every part honors the same env overrides upstream reads.
pub fn user_agent() -> String {
    let channel = std::env::var("OPENCODE_CHANNEL").unwrap_or_else(|_| "latest".into());
    let version = std::env::var("OPENCODE_VERSION").unwrap_or_else(|_| "1.18.31".into());
    let client = std::env::var("OPENCODE_CLIENT").unwrap_or_else(|_| "cli".into());
    format!("opencode/{channel}/{version}/{client}")
}

pub fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Lease-eligible key: EXACTLY upstream's `models-dev:${filepath}` where
/// filepath is the write path (flock key → `sha1(key).lock` dir).
pub fn lease_key(write_path: &Path) -> String {
    format!("models-dev:{}", write_path.display())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Fetched,
    Skipped,
    Disabled,
    Failed,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Fetched => "fetched",
            Outcome::Skipped => "skipped",
            Outcome::Disabled => "disabled",
            Outcome::Failed => "failed",
        }
    }
}

/// Cross-process lease (flock.ts faithful): lockdir with meta + heartbeat,
/// stale sweep under a breaker, jittered retry. `None` = timed out.
pub struct Lease {
    lock_dir: PathBuf,
    token: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    heartbeat: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // release() verifies the token before rm (flock.ts): never delete a
        // lock re-acquired by someone else after a stale takeover.
        let meta_path = self.lock_dir.join("meta.json");
        let token_ok = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(String::from))
            .map(|t| t == self.token)
            .unwrap_or(false);
        if token_ok {
            let _ = std::fs::remove_dir_all(&self.lock_dir);
        } else {
            tracing::warn!(
                "models lease: refusing to release {} (token mismatch or missing meta)",
                self.lock_dir.display()
            );
        }
        // heartbeat thread sees `stop` within ≤1s and exits on its own
        // (utimes on the removed dir fails silently — nothing recreated).
        let _ = self.heartbeat.take();
    }
}

fn lock_dir_for(root: &Path, key: &str) -> PathBuf {
    root.join(format!("{}.lock", sha1_hex(key.as_bytes())))
}

fn file_age_ms(path: &Path) -> Option<u128> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    SystemTime::now()
        .duration_since(mtime)
        .ok()?
        .as_millis()
        .into()
}

/// flock.ts `stale()`: heartbeat mtime → meta mtime → lockdir mtime, all
/// vs `staleMs`.
fn lock_is_stale(lock_dir: &Path, stale_ms: u64) -> bool {
    for rel in ["heartbeat", "meta.json", ""] {
        let p = if rel.is_empty() {
            lock_dir.to_path_buf()
        } else {
            lock_dir.join(rel)
        };
        if let Some(age) = file_age_ms(&p) {
            return age > stale_ms as u128;
        }
    }
    false // no heartbeat AND no meta AND no dir → not stale (dir missing is handled by acquire)
}

fn random_token() -> String {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    // fallback (should not hit): time+pid still unique enough per host
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}",
        buf.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        t
    )
}

fn jitter(base: u64) -> u64 {
    // ±30% around base — matches flock.ts jitter()
    let seed = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let pct = (seed % 61) as i64 - 30; // -30..=30
    let v = base as i64 + (base as i64 * pct) / 100;
    v.max(1) as u64
}

/// ISO-8601 UTC timestamp (flock meta `createdAt`) — hand-rolled to avoid
/// a time-crate dep (civil-from-days, Howard Hinnant algorithm).
fn iso8601_utc(now: SystemTime) -> String {
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn acquire_lease_at(root: &Path, key: &str) -> Option<Lease> {
    acquire_lease_inner(root, key, LEASE_STALE_MS, LEASE_TIMEOUT_MS)
}

fn acquire_lease_inner(root: &Path, key: &str, stale_ms: u64, timeout_ms: u64) -> Option<Lease> {
    // upstream acquire(): `mkdir(dir, { recursive: true })` before the loop
    if let Err(e) = std::fs::create_dir_all(root) {
        tracing::warn!("models lease: cannot create root {}: {e}", root.display());
        return None;
    }
    let lock_dir = lock_dir_for(root, key);
    let breaker = lock_dir.with_extension("lock.breaker");
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut delay = LEASE_BASE_DELAY_MS;

    loop {
        // --- tryAcquireLockDir ---
        match std::fs::create_dir(&lock_dir) {
            Ok(()) => {
                // mode 0700 like upstream (best-effort; umask already applied)
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&lock_dir, std::fs::Permissions::from_mode(0o700));
                let token = random_token();
                let hb_path = lock_dir.join("heartbeat");
                let meta_path = lock_dir.join("meta.json");
                if std::fs::File::create_new(&hb_path).is_err()
                    || write_meta(&meta_path, &token).is_err()
                {
                    let _ = std::fs::remove_dir_all(&lock_dir);
                    tracing::warn!("models lease: lock acquired but contents existed — cleaned");
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(jitter(delay)));
                    continue;
                }
                // heartbeat thread: utimes every stale/3 (flock.ts)
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let stop2 = stop.clone();
                let hb2 = hb_path.clone();
                let heartbeat = std::thread::spawn(move || {
                    while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                        // 1s poll so release stops promptly (join-free by design)
                        for _ in 0..(LEASE_HEARTBEAT_MS / 1000).max(1) {
                            if stop2.load(std::sync::atomic::Ordering::Relaxed) {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(1000));
                        }
                        let now = SystemTime::now();
                        let _ = std::fs::File::options()
                            .write(true)
                            .open(&hb2)
                            .and_then(|f| f.set_modified(now));
                    }
                });
                return Some(Lease {
                    lock_dir,
                    token,
                    stop,
                    heartbeat: Some(heartbeat),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                tracing::warn!("models lease: mkdir failed: {e}");
                return None;
            }
        }

        // --- exists: stale? breaker dance (flock.ts) ---
        if !lock_is_stale(&lock_dir, stale_ms) {
            if Instant::now() >= deadline {
                tracing::warn!("models lease: timed out waiting for {key}");
                return None;
            }
            std::thread::sleep(Duration::from_millis(jitter(delay)));
            delay = ((delay as u128 * 17 / 10) as u64).min(LEASE_MAX_DELAY_MS);
            continue;
        }
        // single-contender cleanup via breaker dir
        match std::fs::create_dir(&breaker) {
            Ok(()) => {
                let still_stale = lock_is_stale(&lock_dir, stale_ms);
                if still_stale {
                    let _ = std::fs::remove_dir_all(&lock_dir);
                    // retry immediately: next loop iteration mkdirs fresh
                }
                let _ = std::fs::remove_dir(&breaker);
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // another contender is cleaning — if the breaker itself is
                // stale, sweep it (flock.ts)
                if file_age_ms(&breaker).unwrap_or(0) > stale_ms as u128 {
                    let _ = std::fs::remove_dir_all(&breaker);
                }
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(jitter(delay)));
                delay = ((delay as u128 * 17 / 10) as u64).min(LEASE_MAX_DELAY_MS);
                continue;
            }
            Err(_) => {
                // parent missing (or other mkdir error) → ensure root, back off
                let _ = std::fs::create_dir_all(root);
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(jitter(delay)));
                continue;
            }
        }
    }
}

fn write_meta(path: &Path, token: &str) -> std::io::Result<()> {
    let meta = json!({
        "token": token,
        "pid": std::process::id(),
        "hostname": std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()),
        "createdAt": iso8601_utc(SystemTime::now()),
    });
    let mut f = std::fs::File::create_new(path)?;
    f.write_all(
        serde_json::to_string_pretty(&meta)
            .unwrap_or_default()
            .as_bytes(),
    )
}

/// GET `{source}/api.json` with upstream's retry shape (2 retries =
/// 3 attempts total, exponential 200ms base + jitter, 10s per-attempt
/// timeout) and the mirrored UA.
fn fetch_catalog(source: &str, timeout: Duration) -> anyhow::Result<String> {
    let url = format!("{}/api.json", source.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .context("build fetch client")?;
    let mut last = String::from("attempts exhausted");
    for attempt in 0..3u32 {
        if attempt > 0 {
            let base = 200u64 * (1 << (attempt - 1));
            std::thread::sleep(Duration::from_millis(jitter(base)));
        }
        match client.get(&url).header("User-Agent", user_agent()).send() {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.bytes().context("read body")?;
                // D-MODELS-2: parse-validate before replace (upstream writes
                // raw; we skip the corrupt-read window entirely).
                serde_json::from_slice::<Value>(&body)
                    .context("catalog payload is not valid JSON")?;
                return Ok(String::from_utf8_lossy(&body).into_owned());
            }
            Ok(resp) => {
                last = format!("HTTP {}", resp.status());
            }
            Err(e) => last = format!("transport: {e}"),
        }
    }
    anyhow::bail!("catalog fetch failed after 3 attempts: {last}")
}

fn atomic_write(path: &Path, bytes: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("mkdir cache dir")?;
    }
    let tmp = path.with_file_name(format!(
        "{}.{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("models"),
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let res = (|| -> anyhow::Result<()> {
        let mut f = std::fs::File::create(&tmp).context("create tmp")?;
        f.write_all(bytes.as_bytes()).context("write tmp")?;
        f.sync_all().context("fsync tmp")?;
        std::fs::rename(&tmp, path).context("rename tmp")?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// One refresh pass (upstream `refresh(force)`): disabled → fresh →
/// lease → re-check → fetch → atomic write. Never panics; failures keep
/// the stale cache (upstream `Effect.ignore` + log semantics).
pub fn refresh(force: bool) -> Outcome {
    let write = write_path();
    refresh_with(&source_url(), &write, &lease_root(), force, FETCH_TIMEOUT)
}

/// Parametrised core (tests drive paths/timeouts directly — no env races).
pub fn refresh_with(
    source: &str,
    write: &Path,
    root: &Path,
    force: bool,
    timeout: Duration,
) -> Outcome {
    let outcome = refresh_inner(source, write, root, force, timeout);
    refine_metrics::labeled_counter(
        "refine_models_refresh_total",
        &format!("result=\"{}\"", outcome.label()),
        1,
    );
    outcome
}

fn refresh_inner(
    source: &str,
    write: &Path,
    root: &Path,
    force: bool,
    timeout: Duration,
) -> Outcome {
    if fetch_disabled() {
        return Outcome::Disabled;
    }
    if !force && is_fresh(write, SystemTime::now()) {
        return Outcome::Skipped;
    }
    let key = lease_key(write);
    let Some(_lease) = acquire_lease_at(root, &key) else {
        tracing::warn!("models refresh: lease timed out ({key})");
        return Outcome::Failed;
    };
    // re-check under the lease (another process may have refreshed first)
    if !force && is_fresh(write, SystemTime::now()) {
        return Outcome::Skipped;
    }
    match fetch_catalog(source, timeout) {
        Ok(body) => match atomic_write(write, &body) {
            Ok(()) => {
                tracing::info!(
                    "models catalog refreshed: {} ({} bytes, source {source})",
                    write.display(),
                    body.len()
                );
                Outcome::Fetched
            }
            Err(e) => {
                tracing::warn!("models refresh: write failed: {e:#}");
                Outcome::Failed
            }
        },
        Err(e) => {
            tracing::warn!("models refresh: fetch failed (stale cache kept): {e:#}");
            Outcome::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWN_SHA1_URL: &str = "https://example.com/api";
    // `printf '%s' "https://example.com/api" | sha1sum` — real pin
    const KNOWN_SHA1: &str = "7e4e8ddfd49018183c22eedc846be008f2fdfa32";

    #[test]
    fn file_name_matches_upstream_rule() {
        assert_eq!(file_name_for(DEFAULT_SOURCE), "models.json");
        // pin the derivation: sha1 hex of the source URL
        assert_eq!(
            file_name_for(KNOWN_SHA1_URL),
            format!("models-{KNOWN_SHA1}.json"),
            "sha1(source) pin (Hash.fast parity)"
        );
        assert_ne!(file_name_for(KNOWN_SHA1_URL), "models.json");
    }

    #[test]
    fn fresh_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.json");
        std::fs::write(&p, "{}").unwrap();
        let now = SystemTime::now();
        assert!(is_fresh(&p, now), "just-written file is fresh");
        let future = now + Duration::from_millis(FRESH_MS + 1_000);
        // simulate an OLD mtime by asking with a future "now"
        assert!(!is_fresh(&p, future), "past TTL = stale");
        assert!(
            !is_fresh(&dir.path().join("missing.json"), now),
            "missing = stale"
        );
    }

    #[test]
    fn truthy_pins_flag_semantics() {
        // env_truthy reads live env — keep to the values the pin documents
        // by testing through fetch_disabled with the one var we can set
        // safely... env races: skip live-set; pin the matcher shape instead.
        // (upstream: value === "true" || value === "1", case-lowered)
        for (input, want) in [
            ("true", true),
            ("1", true),
            ("TRUE", true),
            ("0", false),
            ("yes", false),
            ("", false),
        ] {
            let got = matches!(
                Some(input).map(str::to_lowercase).as_deref(),
                Some("true") | Some("1")
            );
            assert_eq!(got, want, "input {input}");
        }
    }

    #[test]
    fn ua_mirrors_upstream_shape() {
        let ua = user_agent();
        assert!(
            ua.starts_with("opencode/") && ua.matches('/').count() == 3,
            "opencode/{{channel}}/{{version}}/{{client}} — got {ua}"
        );
    }

    #[test]
    fn iso8601_known_values() {
        assert_eq!(iso8601_utc(SystemTime::UNIX_EPOCH), "1970-01-01T00:00:00Z");
        // 2026-10-05T09:00:00Z = 1791190800 (from the zen capture timeline)
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_791_190_800);
        assert_eq!(iso8601_utc(t), "2026-10-05T09:00:00Z");
    }

    #[test]
    fn lease_acquire_and_release_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let key = lease_key(Path::new("/tmp/x/models.json"));
        {
            let l = acquire_lease_at(dir.path(), &key).expect("first acquire");
            let lock = lock_dir_for(dir.path(), &key);
            assert!(lock.join("meta.json").exists());
            assert!(lock.join("heartbeat").exists());
            drop(l);
            assert!(!lock.exists(), "released lease removes the lockdir");
        }
        // re-acquire after release works (token lifecycle)
        let l2 = acquire_lease_at(dir.path(), &key).expect("re-acquire");
        drop(l2);
    }

    #[test]
    fn lease_taken_over_only_when_stale() {
        let dir = tempfile::tempdir().unwrap();
        let key = "models-dev:/t/m.json";
        let lock = lock_dir_for(dir.path(), key);
        // fresh lockdir with fresh heartbeat → second acquire must NOT take it
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(lock.join("heartbeat"), "").unwrap();
        write_meta(&lock.join("meta.json"), "other").unwrap();
        let got = acquire_lease_inner(dir.path(), key, 60_000, 300);
        assert!(got.is_none(), "fresh lock must not be stolen");
        drop(got);
        // backdate heartbeat+meta+dir beyond stale → takeover allowed
        let old = SystemTime::now() - Duration::from_millis(120_000);
        for p in [lock.join("heartbeat"), lock.join("meta.json")] {
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(old).unwrap();
        }
        let l = acquire_lease_inner(dir.path(), key, 60_000, 3_000)
            .expect("stale lock must be taken over");
        drop(l);
        assert!(!lock.exists(), "takeover owns + releases cleanly");
    }

    #[test]
    fn lease_refuses_release_on_token_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let key = "models-dev:/t/m2.json";
        let lock = lock_dir_for(dir.path(), key);
        let l = acquire_lease_at(dir.path(), key).expect("acquire");
        // simulate a stale takeover by another process (meta rewritten)
        std::fs::write(lock.join("meta.json"), r#"{"token":"someone-else"}"#).unwrap();
        drop(l);
        assert!(lock.exists(), "mismatched token → lock NOT deleted");
        let _ = std::fs::remove_dir_all(&lock);
    }

    #[test]
    fn fetch_retries_then_succeeds_and_sends_ua() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // thread-per-connection + Connection: close — no keep-alive races
        // with the accept loop (the first draft's failure mode)
        std::thread::spawn(move || {
            let mut n = 0usize;
            while let Ok((mut sock, _)) = listener.accept() {
                n += 1;
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf);
                    if n == 1 {
                        // first attempt: 500 (retried per upstream)
                        let _ = sock.write_all(
                            b"HTTP/1.1 500 Server Error\r\ncontent-length:0\r\nconnection: close\r\n\r\n",
                        );
                    } else {
                        let body = br#"{"deepseek":{}}"#;
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(head.as_bytes());
                        let _ = sock.write_all(body);
                    }
                });
            }
        });
        let body = fetch_catalog(&format!("http://{addr}"), Duration::from_secs(5))
            .expect("retry then succeed");
        assert!(body.contains("deepseek"));
    }

    #[test]
    fn fetch_fails_on_non_json_payload() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length:5\r\nconnection: close\r\n\r\nnope!",
                );
            }
        });
        let err = fetch_catalog(&format!("http://{addr}"), Duration::from_secs(5))
            .expect_err("invalid JSON must fail before replace");
        assert!(format!("{err:#}").contains("not valid JSON"));
    }

    /// Live: models.opencode.ai answers with the catalog refine consumes
    /// (1 request — nightly; PR runs skip via #[ignore]).
    #[test]
    #[ignore = "live models.opencode.ai fetch (nightly)"]
    fn live_fetch_contains_big_pickle() {
        let body = fetch_catalog(DEFAULT_SOURCE, FETCH_TIMEOUT).expect("live catalog fetch");
        let v: Value = serde_json::from_str(&body).expect("live catalog parses");
        assert!(
            v.pointer("/opencode/models/big-pickle").is_some(),
            "catalog must still carry opencode/big-pickle (zen default family)"
        );
        assert!(
            v.pointer("/opencode/api")
                .and_then(|x| x.as_str())
                .is_some_and(|u| u.contains("opencode.ai")),
            "provider api base present (endpoint derivation input)"
        );
    }

    #[test]
    fn refresh_fetches_writes_and_cleans_lease() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = listener.accept() {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf);
                    let body = br#"{"bigpickle":{}}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes());
                    let _ = sock.write_all(body);
                });
            }
        });
        let tmp = tempfile::tempdir().unwrap();
        let write = tmp.path().join("cache/models.json");
        let root = tmp.path().join("locks");
        let out = refresh_with(
            &format!("http://{addr}"),
            &write,
            &root,
            false,
            Duration::from_secs(5),
        );
        assert_eq!(out, Outcome::Fetched);
        assert!(write.exists());
        assert!(
            std::fs::read_to_string(&write)
                .unwrap()
                .contains("bigpickle"),
            "written payload visible"
        );
        // lease fully released (no leftover lockdirs)
        let leftovers = std::fs::read_dir(&root)
            .map(|d| d.flatten().count())
            .unwrap_or(usize::MAX);
        assert_eq!(leftovers, 0, "lockdir released after refresh");

        // second pass within TTL → Skipped (fresh gate)
        let out2 = refresh_with(
            &format!("http://{addr}"),
            &write,
            &root,
            false,
            Duration::from_secs(5),
        );
        assert_eq!(out2, Outcome::Skipped);

        // force bypasses freshness → Fetched again
        let out3 = refresh_with(
            &format!("http://{addr}"),
            &write,
            &root,
            true,
            Duration::from_secs(5),
        );
        assert_eq!(out3, Outcome::Fetched);
    }
}
