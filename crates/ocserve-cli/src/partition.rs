//! L2 per-component cgroup partition (SRE §5 middle-ground stack).
//!
//! Goal: children (plugin sidecar, browser host, MCP servers) get a chosen
//! ceiling (`kids/memory.max`, default 700M — covers the measured 614M
//! embedding burst) while main's headroom under the unit `MemoryMax`
//! becomes a *guarantee* instead of an `oom_score_adj` lottery.
//!
//! Kernel constraints this encodes (admin-guide/cgroup-v2 §No Internal
//! Process Constraint): a non-root cgroup with processes cannot enable
//! domain controllers in its `subtree_control` — so the dance is
//! `mkdir main kids → move self into main → +memory → cap kids`.
//!
//! Scope gate: the dance mutates WHATEVER cgroup the process lives in, and
//! tests/replay/foreground runs may live in a terminal's or harness's
//! cgroup. So it runs only when `OCSERVE_CGROUP_PARTITION=1` (force), or when
//! unset and `INVOCATION_ID` is present (systemd launched this unit).
//! `OCSERVE_CGROUP_PARTITION=0` always disables (kill switch). Anything else
//! = Disabled: flat shared-cap behavior, one metric, silent.

use std::path::{Path, PathBuf};

/// Published by `setup()` on success so the sampler can gauge
/// `ocserve_kids_bytes`; unset (or `Disabled`/`Unavailable`) = no kids gauge.
static KIDS: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn kids_path() -> Option<&'static Path> {
    KIDS.get().map(|p| p.as_path())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Partition {
    Disabled,
    Unavailable(&'static str),
    Ok,
}

/// Every failure and success reason — stable strings (metric/log labels).
pub fn setup() -> &'static str {
    let result = outcome(enabled(), our_cgroup_root());
    label_and_publish(result)
}

/// Glue seam (tested — the live battery caught the first version passing
/// `enabled()` into setup_in's `disabled` parameter, inverting the gate):
/// enabled → dance against root; disabled → zero mutation; no v2 root →
/// degrade.
fn outcome(enabled: bool, root: Option<PathBuf>) -> Partition {
    match root {
        None => Partition::Unavailable("no-v2-cgroup"),
        Some(root) => setup_in(&root, !enabled),
    }
}

fn label_and_publish(result: Partition) -> &'static str {
    let label = match &result {
        Partition::Ok => "ok",
        Partition::Disabled => "disabled",
        Partition::Unavailable(why) => {
            tracing::debug!("cgroup partition unavailable: {why}");
            "unavailable"
        }
    };
    ocserve_metrics::labeled_counter(
        "ocserve_cgroup_partition_total",
        &format!("result=\"{label}\""),
        1,
    );
    match &result {
        Partition::Ok => tracing::info!(
            "cgroup partition: kids ceiling active ({:?})",
            KIDS.get()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ),
        Partition::Disabled => tracing::debug!("cgroup partition disabled"),
        Partition::Unavailable(_) => {}
    }
    label
}

fn enabled() -> bool {
    enabled_from(
        std::env::var("OCSERVE_CGROUP_PARTITION").ok().as_deref(),
        std::env::var("INVOCATION_ID").ok().as_deref(),
    )
}

/// Pure gate (tested): `OCSERVE_CGROUP_PARTITION=0` kills, `=1` forces,
/// unset/other → only under systemd (`INVOCATION_ID`) — bare/test/harness
/// runs must never restructure a terminal's or cargo's cgroup tree.
fn enabled_from(env: Option<&str>, invocation_id: Option<&str>) -> bool {
    match env {
        Some("0") => false,
        Some("1") => true,
        _ => invocation_id.is_some(),
    }
}

/// `0::/path` → `/sys/fs/cgroup/path` (relative join — `Path::join` with an
/// absolute arg would REPLACE the root).
fn our_cgroup_root() -> Option<PathBuf> {
    let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = cg.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        // process sits in the cgroup root — restructuring the hierarchy
        // from a non-init process is never ours to do
        return None;
    }
    Some(PathBuf::from("/sys/fs/cgroup").join(rel))
}

/// The dance against an explicit root (fake-fs testable; the real root in
/// prod). `disabled` is computed by the caller so tests pass it directly.
pub fn setup_in(root: &Path, disabled: bool) -> Partition {
    if disabled {
        return Partition::Disabled;
    }
    // probe chain — first failure wins, nothing is mutated before it
    let Ok(md) = std::fs::metadata(root) else {
        return Partition::Unavailable("root-missing");
    };
    if !md.is_dir() {
        return Partition::Unavailable("root-not-dir");
    }
    // ownership: only restructure cgroup dirs we own (delegated user tree);
    // anything root-owned (system units, containers) → degrade flat
    let Some(uid) = our_uid_opt() else {
        return Partition::Unavailable("uid-unknown");
    };
    if !owned_by_uid(&md, uid) {
        return Partition::Unavailable("not-owned-by-us");
    }
    let Ok(controllers) = std::fs::read_to_string(root.join("cgroup.controllers")) else {
        return Partition::Unavailable("no-controllers-file");
    };
    if !controllers.split_whitespace().any(|c| c == "memory") {
        return Partition::Unavailable("no-memory-controller");
    }
    // subtree writable? opening without writing = non-mutating probe
    if std::fs::OpenOptions::new()
        .write(true)
        .open(root.join("cgroup.subtree_control"))
        .is_err()
    {
        return Partition::Unavailable("subtree-not-writable");
    }

    // dance
    if let Err(e) = std::fs::create_dir_all(root.join("main")) {
        tracing::warn!("partition: mkdir main failed: {e}");
        return Partition::Unavailable("mkdir-main");
    }
    if let Err(e) = std::fs::create_dir_all(root.join("kids")) {
        tracing::warn!("partition: mkdir kids failed: {e}");
        return Partition::Unavailable("mkdir-kids");
    }
    let pid = std::process::id();
    if let Err(e) = std::fs::write(root.join("main/cgroup.procs"), pid.to_string()) {
        tracing::warn!("partition: self-move failed: {e}");
        return Partition::Unavailable("self-move");
    }
    // now this cgroup has no processes → kernel allows enabling controllers
    if let Err(e) = enable(
        root.join("cgroup.subtree_control").as_path(),
        &["memory", "pids"],
    ) {
        tracing::warn!("partition: enable controllers failed: {e}");
        return Partition::Unavailable("enable-memory");
    }
    let mb: u64 = std::env::var("OCSERVE_KIDS_MEMORY_MAX_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(700);
    let max_path = root.join("kids/memory.max");
    if let Err(e) = std::fs::write(&max_path, (mb * 1024 * 1024).to_string()) {
        tracing::warn!("partition: kids memory.max failed: {e}");
        return Partition::Unavailable("kids-cap");
    }
    let kids = root.join("kids");
    let _ = KIDS.set(kids.clone());

    // Per-class leaves. `kids/` is a set of independent services, so
    // `oom.group=1` belongs on each leaf (kill this class as a unit), never on
    // the aggregate: that would kill the browser host because a build spiked.
    // Enabling controllers on kids/ requires it to have no processes — true at
    // boot; if it ever is not (a stale hierarchy), degrade to the flat kids cap
    // rather than failing the service.
    let leaves_ok = enable(
        kids.join("cgroup.subtree_control").as_path(),
        &["memory", "pids"],
    )
    .is_ok();
    if leaves_ok {
        for name in ocserve_store::cgroup::LEAVES {
            let leaf = kids.join(name);
            if let Err(e) = std::fs::create_dir_all(&leaf) {
                tracing::warn!("partition: mkdir kids/{name} failed: {e}");
                continue;
            }
            // kill the whole class together; partial death is worse for a
            // single service than total death (kernel cgroup-v2 §memory.oom.group)
            write_or_warn(&leaf.join("memory.oom.group"), "1".to_string());
            // fork-bomb guard: a tool call cannot outrun the class budget
            write_or_warn(&leaf.join("pids.max"), "1024".to_string());
        }
    } else {
        tracing::warn!(
            "partition: kids subtree not delegable (processes present) — flat kids cap only"
        );
    }
    // Protection, not a cap: under pressure the kernel reclaims from kids
    // first. 320M matches main's measured 196-223MB idle envelope with margin.
    write_or_warn(
        &root.join("main/memory.low"),
        (320u64 * 1024 * 1024).to_string(),
    );

    ocserve_store::cgroup::publish(root);
    log_budget(root, &kids, mb);
    Partition::Ok
}

/// Enable controllers in a `cgroup.subtree_control` (idempotent, additive).
fn enable(sc: &Path, wanted: &[&str]) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(sc).unwrap_or_default();
    let missing: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|w| {
            !existing
                .split_whitespace()
                .any(|c| c.trim_start_matches('+') == *w)
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut next = existing.trim().to_string();
    for w in missing {
        if !next.is_empty() {
            next.push(' ');
        }
        next.push('+');
        next.push_str(w);
    }
    std::fs::write(sc, next)
}

fn write_or_warn(path: &Path, value: String) {
    if let Err(e) = std::fs::write(path, value) {
        tracing::warn!("partition: write {} failed: {e}", path.display());
    }
}

/// Say the budget out loud instead of implying it in a comment. The 2026-10-21
/// kill was caused by placement, but the arithmetic was structurally
/// over-committed: kids 700 + main 320 exceeds the 1024 parent before slab.
/// Reading the numbers from the kernel keeps this honest if a knob changes.
fn log_budget(root: &Path, kids: &Path, kids_mb: u64) {
    let parent_mb = std::fs::read_to_string(root.join("memory.max"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|b| b / 1024 / 1024);
    let main_low_mb = std::fs::read_to_string(root.join("main/memory.low"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
        / 1024
        / 1024;
    let leaves = ocserve_store::cgroup::LEAVES
        .iter()
        .filter(|n| kids.join(n).is_dir())
        .count();
    match parent_mb {
        Some(p) => {
            // 64 MB is the working reserve for slab, page tables and the page
            // cache the server actually touches; a budget that fits only by a
            // few MB is over-committed in practice (2026-10-21: kids 700 +
            // main 320 against 1024 leaves 4 MB).
            const HEADROOM_MB: u64 = 64;
            let committed = kids_mb + main_low_mb;
            if committed + HEADROOM_MB > p {
                tracing::warn!(
                    "memory budget tight: parent {p}M, kids.max {kids_mb}M + main.low \
                     {main_low_mb}M leaves {}M for slab/cache (want {HEADROOM_MB}M) — kids \
                     is reclaimed first so main survives; SRE §5.5",
                    p.saturating_sub(committed)
                );
            } else {
                tracing::info!(
                    "memory budget: parent {p}M, kids.max {kids_mb}M + main.low {main_low_mb}M, \
                     {}M headroom",
                    p - committed
                );
            }
        }
        None => tracing::info!(
            "memory budget: kids.max {kids_mb}M, main.low {main_low_mb}M, {leaves} class leaves"
        ),
    }
}

/// Ownership test. Unix has `MetadataExt::uid`; elsewhere there is no uid and
/// the cgroup partition is unavailable anyway (`our_uid_opt` reads /proc), so
/// this returns false rather than pretending an ACL check happened.
fn owned_by_uid(md: &std::fs::Metadata, uid: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        md.uid() == uid
    }
    #[cfg(not(unix))]
    {
        let _ = (md, uid);
        false
    }
}

fn our_uid_opt() -> Option<u32> {
    let st = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = st.lines().find(|l| l.starts_with("Uid:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake cgroup dir: controllers + writable subtree + our uid ownership.
    /// Unique per CALL — tests run in parallel and would otherwise nuke
    /// each other's fixture (the exact collision class, caught red here).
    fn fixture(memory: bool) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("ocserve-partition-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("cgroup.controllers"),
            if memory {
                "cpuset memory pids"
            } else {
                "cpuset pids"
            },
        )
        .unwrap();
        std::fs::write(root.join("cgroup.subtree_control"), "").unwrap();
        root
    }

    #[test]
    fn leaves_are_created_and_capped() {
        let root = fixture(true);
        assert_eq!(setup_in(&root, false), Partition::Ok);
        for name in ocserve_store::cgroup::LEAVES {
            let leaf = root.join("kids").join(name);
            assert!(leaf.is_dir(), "leaf {name} missing");
            assert_eq!(
                std::fs::read_to_string(leaf.join("memory.oom.group")).unwrap(),
                "1",
                "leaf {name} must die as a unit"
            );
            assert_eq!(
                std::fs::read_to_string(leaf.join("pids.max")).unwrap(),
                "1024",
                "leaf {name} needs a fork-bomb bound"
            );
        }
        // Aggregate must NOT carry oom.group: kids/ is a set of independent
        // services, so a build spiking must not take the browser host with it.
        assert!(
            !root.join("kids/memory.oom.group").exists(),
            "kids/ must not be an oom group"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("main/memory.low")).unwrap(),
            (320u64 * 1024 * 1024).to_string(),
            "main must be the reclaim-protected side"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn happy_path_dance_and_caps() {
        let root = fixture(true);
        let p = setup_in(&root, false);
        assert_eq!(p, Partition::Ok);
        assert_eq!(
            std::fs::read_to_string(root.join("kids/memory.max")).unwrap(),
            (700u64 * 1024 * 1024).to_string(),
            "kids ceiling must be 700M in bytes (kernel takes plain bytes)"
        );
        let sc = std::fs::read_to_string(root.join("cgroup.subtree_control")).unwrap();
        assert!(
            sc.contains("+memory"),
            "subtree_control must enable memory: {sc}"
        );
        let procs = std::fs::read_to_string(root.join("main/cgroup.procs")).unwrap();
        assert_eq!(
            procs.trim(),
            std::process::id().to_string(),
            "self must be in main/"
        );
        assert!(setup_in(&root, false) == Partition::Ok, "idempotent re-run"); // KIDS set-once is fine
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn glue_maps_enabled_to_dance_and_disabled_to_no_mutation() {
        let root = fixture(true);
        // enabled + real-ish root → Ok (this is the inversion the battery caught)
        assert_eq!(outcome(true, Some(root.clone())), Partition::Ok);
        assert!(root.join("kids/memory.max").exists());
        let _ = std::fs::remove_dir_all(&root);

        let root = fixture(true);
        assert_eq!(outcome(false, Some(root.clone())), Partition::Disabled);
        assert!(!root.join("main").exists(), "disabled must not mutate");
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(outcome(true, None), Partition::Unavailable("no-v2-cgroup"));
    }

    #[test]
    fn kill_switch_disables() {
        let root = fixture(true);
        assert_eq!(setup_in(&root, true), Partition::Disabled);
        assert!(!root.join("main").exists(), "disabled = zero mutation");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_failures_degrade_without_mutation() {
        let root = fixture(true);
        assert_eq!(
            setup_in(&root.join("nope"), false),
            Partition::Unavailable("root-missing")
        );
        let _ = std::fs::remove_dir_all(&root);

        let root = fixture(false);
        assert_eq!(
            setup_in(&root, false),
            Partition::Unavailable("no-memory-controller")
        );
        assert!(!root.join("main").exists(), "failed probe must not mutate");
        let _ = std::fs::remove_dir_all(&root);

        let root = fixture(true);
        std::fs::remove_file(root.join("cgroup.controllers")).unwrap();
        assert_eq!(
            setup_in(&root, false),
            Partition::Unavailable("no-controllers-file")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn subtree_readonly_degrades() {
        let root = fixture(true);
        let sc = root.join("cgroup.subtree_control");
        let f = std::fs::File::create(&sc).unwrap();
        drop(f);
        use std::os::unix::fs::PermissionsExt;
        let mut ro = std::fs::metadata(&sc).unwrap().permissions();
        ro.set_mode(0o444);
        std::fs::set_permissions(&sc, ro).unwrap();
        assert_eq!(
            setup_in(&root, false),
            Partition::Unavailable("subtree-not-writable")
        );
        assert!(!root.join("main").exists());
        let mut rw = std::fs::metadata(&sc).unwrap().permissions();
        rw.set_mode(0o644);
        std::fs::set_permissions(&sc, rw).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enabled_requires_force_or_systemd_invocation() {
        // bare/test/harness (no env, no INVOCATION_ID) → NEVER restructures
        assert!(!enabled_from(None, None));
        // kill switch wins over systemd
        assert!(!enabled_from(Some("0"), Some("some-invocation")));
        // force works without systemd (manual experiments)
        assert!(enabled_from(Some("1"), None));
        // systemd default: on
        assert!(enabled_from(None, Some("some-invocation")));
        // junk env = unset (never panics, never guesses)
        assert!(!enabled_from(Some("junk"), None));
        assert!(enabled_from(Some("junk"), Some("id")));
    }
}
