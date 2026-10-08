//! H1: config hot-reload for EXTERNAL edits (vim/TUI writing the shared
//! files) + the single reconcile pipeline that PATCH/auth paths also use.
//!
//! Divergence D-CONFIG-1 (TESTING §1.6): upstream has NO file watcher —
//! config is `Effect.cachedInvalidateWithTTL(Duration.infinity)` invalidated
//! only on its own write (v1 config/config.ts:295,302,678), so an external
//! edit does not apply until restart. ocserve applies it within one poll
//! interval. Shapes/routes unchanged; replay unaffected (file state is fixed
//! during a recording).
//!
//! Design (antagonism decisions):
//! - **Poll, not notify/inotify**: editors save via tmp+rename (inotify on
//!   the file loses the watch; dir-watch needs debounce + a dependency);
//!   stat-ing ≤6 known files every interval is bounded, rename-proof,
//!   zero-dependency. Interval clamped ≥100ms; `OCSERVE_CONFIG_WATCH=0`
//!   disables; `OCSERVE_CONFIG_POLL_MS` tunes (default 2000).
//! - **Fail-safe at runtime, fail-fast at boot** (SRE §1): a broken config
//!   keeps the OLD payloads/registry serving + `result="error"` metric and
//!   self-heals on the next successful load; boot still refuses to start.
//! - **Serialized**: PATCH/auth paths and the watcher task share one
//!   `reconcile_lock` (tokio Mutex — held across the reload + MCP awaits,
//!   never a parking_lot guard).
//! - **Observed tuples recorded only after a successful load** → failed
//!   loads retry every tick until the file is valid (no lost updates, no
//!   write side effects → no feedback loop).
//! - MCP section diffed against the previously SERVED config: added →
//!   upsert_cfg + connect; changed → connect (replaces the client) with the
//!   new cfg; removed/disabled → disconnect + drop_cfg; any change rescans
//!   tool schemas (boot parity: connect-time trust scan).

use crate::AppState;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// (mtime, len) of one watched file; (None, 0) = missing at stat time.
type Observed = (Option<SystemTime>, u64);

/// Return freed-but-retained glibc heap pages to the OS (`malloc_trim(0)`).
///
/// Measured 2026-10-05 (`bench/profiling/OOM-RELOAD-REPORT.md` §Tier-0): the
/// full-catalog build pipeline leaves ~218 MB of FREED pages in the arena —
/// a tiny-catalog control settles at 19 MB RSS while the full 5.3 MB catalog
/// settles at 237 MB, and an LD_PRELOAD trim shim collapses that to 91 MB
/// with reload deltas +3/0/−1 MB. `MALLOC_ARENA_MAX=1` (unit) removed the
/// per-reload *step*; trim removes the high-water *retention*.
///
/// Called at boot, after every reconcile swap, and on a periodic task
/// (`OCSERVE_TRIM_SECS`, default 300 s, 0 disables). `OCSERVE_TRIM=0` turns
/// every call into a no-op (lab A/B knob). No-op on non-glibc targets.
pub fn trim_heap() -> bool {
    if std::env::var("OCSERVE_TRIM").is_ok_and(|v| v == "0") {
        return false;
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // Safety: `malloc_trim` is MT-Safe (glibc); it releases free heap
        // pages and takes no pointers from us. Arena lock is held briefly.
        unsafe {
            libc::malloc_trim(0);
        }
        true
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        false
    }
}

/// Watched-file state. `paths` set once at boot (≤6 files — bounded by the
/// set `Runtime::load_for` reads); `observed` tracks last-seen
/// (mtime,len)+content-hash so the watcher fires only on real CONTENT
/// changes. The hash matters: their opencode rewrites `models.json`
/// ~hourly with IDENTICAL bytes (fresh-TTL fetch) — mtime churn without a
/// content change used to trigger a full reload (the proven warm-kill
/// spike: 7 OOM kills on 2026-10-05, journal 17:01:33) for zero benefit.
#[derive(Default)]
pub struct WatchState {
    pub paths: Vec<PathBuf>,
    observed: HashMap<PathBuf, (Observed, u64)>,
}

fn stat_one(p: &std::path::Path) -> Observed {
    match std::fs::metadata(p) {
        Ok(md) => (md.modified().ok(), md.len()),
        Err(_) => (None, 0),
    }
}

/// Content hash for change detection (only computed when the fast
/// (mtime,len) tuple already differs — never on the steady poll path).
fn content_hash(p: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let Ok(bytes) = std::fs::read(p) else {
        return 0;
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Record current tuples for every watched path (call after each successful
/// load AND once at boot so the first poll tick doesn't re-load fresh state).
pub fn observe(st: &AppState) {
    let paths = st.watch.read().paths.clone();
    let mut w = st.watch.write();
    w.observed = paths
        .iter()
        .map(|p| (p.clone(), (stat_one(p), content_hash(p))))
        .collect();
}

/// True when any watched path's CONTENT differs from the last observed
/// state. (mtime,len) is the fast path; when only mtime churned (same
/// bytes — their hourly identical catalog rewrite) the tuple is advanced
/// in place, `result="skipped"` is counted, and NO reload is requested.
/// Empty watch set → false (no-op).
pub fn changed(st: &AppState) -> bool {
    let mut w = st.watch.write();
    if w.paths.is_empty() {
        return false;
    }
    let paths = w.paths.clone();
    let mut any = false;
    for p in &paths {
        let tuple = stat_one(p);
        match w.observed.get(p) {
            Some((last, _)) if *last == tuple => {}
            Some((_, hash)) => {
                let h = content_hash(p);
                if h == *hash {
                    w.observed.insert(p.clone(), (tuple, h));
                    tracing::info!(
                        "watch: {} mtime changed, content identical — reload skipped",
                        p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    );
                    ocserve_metrics::labeled_counter(
                        "ocserve_config_reload_total",
                        "result=\"skipped\"",
                        1,
                    );
                } else {
                    any = true;
                }
            }
            None => any = true,
        }
    }
    any
}

/// Rebuild payloads + LLM registry from disk and (re)apply both, then diff
/// the MCP section against what was previously served.
///
/// Failure contract: load error → NOTHING is swapped (old config keeps
/// serving), `result="error"` counted, Err returned for the caller to log;
/// observed tuples are NOT advanced so the watcher retries.
pub async fn reconcile(st: &Arc<AppState>) -> anyhow::Result<()> {
    let _guard = st.reconcile_lock.lock().await;
    let Some(reload) = st.reloader.read().clone() else {
        tracing::warn!("config reload: reloader unset (tests/boot) — restart to apply");
        return Ok(());
    };
    // A2 (2026-10-05 OOM postmortem): every reload is now attributable —
    // which files changed, how long it took, and the RSS delta. The old
    // silent reloads hid the catalog-reconcile kill (14:12:21 write → :27).
    let reload_t0 = std::time::Instant::now();
    let rss0 = ocserve_metrics::rss_bytes().unwrap_or(0);
    // capture the previously SERVED mcp section for the diff (before swap)
    let old_mcp = st
        .payloads
        .read()
        .config
        .get("mcp")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let loaded = reload();
    let (payloads, registry) = match loaded {
        Ok(pair) => pair,
        Err(e) => {
            ocserve_metrics::labeled_counter("ocserve_config_reload_total", "result=\"error\"", 1);
            return Err(e.context("config reload"));
        }
    };
    // capture the changed-file set BEFORE observe() advances the tuples
    let changed_files: Vec<String> = {
        let w = st.watch.read();
        w.paths
            .iter()
            .filter(|p| w.observed.get(*p).map(|(t, _)| *t) != Some(stat_one(p)))
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect()
    };
    // F5: re-serialize the wire cache in the SAME swap window as the values
    // (build first from the borrow, then publish both — a reader never sees
    // values without their matching bytes).
    let wire_map = crate::rebuild_wire(
        &payloads,
        st.wire_off.load(std::sync::atomic::Ordering::Relaxed),
    );
    *st.payloads.write() = payloads;
    *st.wire.write() = wire_map;
    *st.llm.write() = registry;
    observe(st); // only after success (retry semantics above)
    // hand the build pipeline's freed pages back (measured ~218 MB high-water)
    trim_heap();
    let rss1 = ocserve_metrics::rss_bytes().unwrap_or(0);
    tracing::info!(
        "config hot-reload ok in {}ms: [{}] rss {:.1} → {:.1} MB",
        reload_t0.elapsed().as_millis(),
        changed_files.join(", "),
        rss0 as f64 / 1_048_576.0,
        rss1 as f64 / 1_048_576.0,
    );

    let new_mcp = st
        .payloads
        .read()
        .config
        .get("mcp")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mcp_changed = if old_mcp != new_mcp {
        reconcile_mcp(st, &old_mcp, &new_mcp).await
    } else {
        false
    };
    ocserve_metrics::labeled_counter(
        "ocserve_config_reload_total",
        if mcp_changed {
            "result=\"mcp\""
        } else {
            "result=\"ok\""
        },
        1,
    );
    Ok(())
}

/// Diff old/new `mcp` config sections against the live hub. Returns true
/// when any server was connected/disconnected/reconnected.
async fn reconcile_mcp(st: &Arc<AppState>, old: &Value, new: &Value) -> bool {
    let to_map = |v: &Value| -> HashMap<String, ocserve_mcp::ServerCfg> {
        ocserve_mcp::parse_config(v)
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect()
    };
    let old_cfgs = to_map(old);
    let new_cfgs = to_map(new);
    let Some(hub) = st.mcp.get().cloned() else {
        tracing::info!("mcp config changed but no hub is set — skipped");
        return false;
    };

    let mut any = false;
    // removed or newly-disabled servers
    for (name, old_cfg) in &old_cfgs {
        let keep = new_cfgs.get(name).is_some_and(|c| c.enabled);
        if old_cfg.enabled && !keep {
            if let Err(e) = hub.disconnect(name).await {
                tracing::debug!("mcp reconcile disconnect {name}: {e:#}"); // absent = fine
            }
            hub.drop_cfg(name);
            any = true;
        }
    }
    // added / changed / newly-enabled servers
    for (name, new_cfg) in &new_cfgs {
        if !new_cfg.enabled {
            continue;
        }
        match old_cfgs.get(name) {
            Some(old_cfg) if old_cfg == new_cfg && old_cfg.enabled => {}
            Some(old_cfg) => {
                // changed definition: connect() replaces the live client,
                // but only AFTER the new cfg is retained (connect reads cfgs)
                if !old_cfg.enabled {
                    // was disabled (client absent) — connect alone is enough
                }
                hub.upsert_cfg(new_cfg.clone());
                if let Err(e) = hub.connect(name).await {
                    tracing::warn!("mcp reconcile connect {name}: {e:#}");
                }
                any = true;
            }
            None => {
                hub.upsert_cfg(new_cfg.clone());
                if let Err(e) = hub.connect(name).await {
                    tracing::warn!("mcp reconcile connect {name}: {e:#}");
                }
                any = true;
            }
        }
    }
    if any {
        // boot parity: enumerate + connect-time trust scan the new surface
        // (connect/disconnect invalidate tools_cache, so this rebuilds it)
        let listed = hub.tool_schemas().await;
        tracing::info!("mcp reconcile: {} tool schemas rescanned", listed.len());
    }
    any
}

/// Watcher knobs. Tests construct these directly (env reads are confined to
/// `from_env` so parallel tests never race `set_var`).
#[derive(Debug, Clone, Copy)]
pub struct WatchOpts {
    pub enabled: bool,
    pub interval_ms: u64,
}

impl Default for WatchOpts {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 2000,
        }
    }
}

impl WatchOpts {
    /// OCSERVE_CONFIG_WATCH=0 disables; OCSERVE_CONFIG_POLL_MS tunes
    /// (default 2000, clamped ≥100ms — 0 must never busy-loop).
    pub fn from_env() -> Self {
        Self {
            enabled: !std::env::var("OCSERVE_CONFIG_WATCH").is_ok_and(|v| v == "0"),
            interval_ms: std::env::var("OCSERVE_CONFIG_POLL_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2000)
                .max(100),
        }
    }
}

/// Watcher task (spawned by serve): poll → changed → reconcile.
/// Owns no state beyond the Arc; exits only on abort/process end.
pub async fn watch_loop(st: Arc<AppState>, opts: WatchOpts) {
    if !opts.enabled {
        tracing::info!("config hot-reload disabled (OCSERVE_CONFIG_WATCH=0)");
        return;
    }
    let interval_ms = opts.interval_ms.max(100); // clamp (defensive for direct callers)
    {
        let w = st.watch.read();
        tracing::info!(
            "config hot-reload: polling {} file(s) every {interval_ms}ms",
            w.paths.len()
        );
    }
    let mut tick = tokio::time::interval(Duration::from_millis(interval_ms));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if changed(&st)
            && let Err(e) = reconcile(&st).await
        {
            tracing::warn!("config hot-reload failed (will retry next tick): {e:#}");
        }
    }
}
