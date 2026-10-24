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
    /// Hierarchy exists, but at least one class cap/weight is missing. The
    /// service boots (never fail on a budget problem) but must not claim "ok":
    /// children in a missing leaf would be uncontained.
    Degraded(&'static str),
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
        Partition::Degraded(why) => {
            tracing::warn!("cgroup partition degraded: {why} — some child classes are uncapped");
            "degraded"
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
        Partition::Degraded(_) => {}
        Partition::Unavailable(_) => {}
    }
    label
}

fn enabled() -> bool {
    let rel = cgroup_rel();
    let exec_pid = std::env::var("SYSTEMD_EXEC_PID")
        .ok()
        .and_then(|v| v.parse::<u32>().ok());
    enabled_from(
        std::env::var("OCSERVE_CGROUP_PARTITION").ok().as_deref(),
        std::env::var("INVOCATION_ID").ok().as_deref(),
        exec_pid,
        std::process::id(),
        rel.as_deref(),
    )
}

/// `/proc/self/cgroup` → the cgroup-v2 path (`0::/a/b/c` → `/a/b/c`).
fn cgroup_rel() -> Option<String> {
    let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    Some(
        cg.lines()
            .find_map(|l| l.strip_prefix("0::"))?
            .trim()
            .to_string(),
    )
}

/// Pure gate (tested): `OCSERVE_CGROUP_PARTITION=0` kills, `=1` forces,
/// unset/other → only as a systemd unit's MAIN process, at its cgroup root.
///
/// `INVOCATION_ID` is inherited by every descendant and by every test server
/// spawned from a unit, so presence alone proved nothing. Two independent
/// conditions are required:
///
/// * `SYSTEMD_EXEC_PID == self` — we ARE the unit's main process. This is what
///   stops a nested `ocserve serve` (started by a tool, or by `cargo test`
///   running inside the user's shell unit) from restructuring its parent's
///   hierarchy.
/// * the cgroup path is a unit root (last component `.service`/`.scope`), not
///   already `main/` or `kids/…`.
///
/// `SYSTEMD_EXEC_PID` exists since systemd 248; when it is absent we decline
/// rather than guess (the dance is optional and already degrades to a flat
/// cap, and a wrong guess mutates somebody else's tree).
fn enabled_from(
    env: Option<&str>,
    invocation_id: Option<&str>,
    exec_pid: Option<u32>,
    self_pid: u32,
    cgroup: Option<&str>,
) -> bool {
    match env {
        Some("0") => false,
        Some("1") => true,
        _ => {
            invocation_id.is_some()
                && exec_pid == Some(self_pid)
                && cgroup.is_some_and(at_unit_root)
        }
    }
}

/// True when the path names a unit root: its last component is a
/// `.service`/`.scope` unit, not a `main`/`kids/<class>` child of one.
fn at_unit_root(path: &str) -> bool {
    let last = path.rsplit('/').find(|c| !c.is_empty()).unwrap_or("");
    last.ends_with(".service") || last.ends_with(".scope")
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
    // CPU is optional: the containment still works without it, but the server
    // cannot be protected from a build that saturates every core. Degrade the
    // weight writes, never the whole dance.
    let has_cpu = controllers.split_whitespace().any(|c| c == "cpu");
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
    // `cpu` joins the same write: weights only work if the controller is
    // enabled on the service cgroup AND on kids/ (2026-10-23: it was absent,
    // so a cargo build competing with the API had no relative priority).
    let mut controllers_wanted = vec!["memory", "pids"];
    if has_cpu {
        controllers_wanted.push("cpu");
    }
    if let Err(e) = enable(
        root.join("cgroup.subtree_control").as_path(),
        &controllers_wanted,
    ) {
        tracing::warn!("partition: enable controllers failed: {e}");
        return Partition::Unavailable("enable-memory");
    }
    let mb: u64 = std::env::var("OCSERVE_KIDS_MEMORY_MAX_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| (32..=1_048_576).contains(v))
        .unwrap_or(700);
    let max_path = root.join("kids/memory.max");
    if let Err(e) = std::fs::write(&max_path, (mb * 1024 * 1024).to_string()) {
        tracing::warn!("partition: kids memory.max failed: {e}");
        return Partition::Unavailable("kids-cap");
    }
    let mut leaf_failures = 0u32;
    // Soft ceiling on the aggregate: throttle the class at 640M instead of
    // letting it run into the 700M hard cap and die as a unit.
    if std::fs::write(
        root.join("kids/memory.high"),
        (640u64 * 1024 * 1024).to_string(),
    )
    .is_err()
    {
        tracing::warn!("partition: write kids/memory.high failed");
        leaf_failures += 1;
    }
    let kids = root.join("kids");
    let _ = KIDS.set(kids.clone());

    // Per-class leaves. `kids/` is a set of independent services, so
    // `oom.group=1` belongs on each leaf (kill this class as a unit), never on
    // the aggregate: that would kill the browser host because a build spiked.
    // Enabling controllers on kids/ requires it to have no processes — true at
    // boot; if it ever is not (a stale hierarchy), degrade to the flat kids cap
    // rather than failing the service.
    let mut kids_controllers = vec!["memory", "pids"];
    if has_cpu {
        kids_controllers.push("cpu");
    }
    let leaves_ok = enable(
        kids.join("cgroup.subtree_control").as_path(),
        &kids_controllers,
    )
    .is_ok();
    if leaves_ok {
        for name in ocserve_store::cgroup::LEAVES {
            let leaf = kids.join(name);
            if let Err(e) = std::fs::create_dir_all(&leaf) {
                tracing::warn!("partition: mkdir kids/{name} failed: {e}");
                leaf_failures += 1;
                continue;
            }
            // Sweep strays: a nested ocserve launched from a tool used to run
            // the dance here and leave empty `main/` / `kids/` dirs behind.
            // Harmless for the +memory gate (that needs no processes, not no
            // children) but they must not accumulate.
            sweep_stray_subdirs(&leaf, name);
            // kill the whole class together; partial death is worse for a
            // single service than total death (kernel cgroup-v2 §memory.oom.group)
            for (file, value) in [
                ("memory.oom.group", "1".to_string()),
                ("pids.max", "1024".to_string()),
            ] {
                if std::fs::write(leaf.join(file), value).is_err() {
                    tracing::warn!("partition: write kids/{name}/{file} failed");
                    leaf_failures += 1;
                }
            }
            // Per-class memory: without this every leaf inherited `max` and a
            // single build could starve the plugin host out of the shared
            // 700M pot (2026-10-23 review). `high < max` so the class is
            // throttled before it is killed.
            let policy = ocserve_store::cgroup::leaf_policy(name);
            for (file, value) in [
                ("memory.high", (policy.high_mb * 1024 * 1024).to_string()),
                ("memory.max", (policy.max_mb * 1024 * 1024).to_string()),
            ] {
                if std::fs::write(leaf.join(file), value).is_err() {
                    tracing::warn!("partition: write kids/{name}/{file} failed");
                    leaf_failures += 1;
                }
            }
            // Relative CPU priority. Weights are work-conserving: the build
            // uses idle cores and yields when the API needs one.
            if has_cpu
                && std::fs::write(leaf.join("cpu.weight"), policy.cpu_weight.to_string()).is_err()
            {
                tracing::warn!("partition: write kids/{name}/cpu.weight failed");
                leaf_failures += 1;
            }
        }
    } else {
        tracing::warn!(
            "partition: kids subtree not delegable (processes present) — flat kids cap only;              children would be uncontained"
        );
        return Partition::Degraded("kids-subtree-not-delegable");
    }
    // The server's own class: a protection floor and a hard backstop.
    //
    // NO `main/memory.high`. `main` contains exactly one process, and
    // `memory.high` throttles allocations by putting the allocating task into
    // direct reclaim. With MemorySwapMax=0 and almost all anon pages, that
    // reclaim cannot make progress: on 2026-10-24 main sat at 601 MiB against
    // a 512 MiB high with 325,225 high events, 4 threads parked in
    // mem_cgroup_handle_over_high, and the server answered nothing while the
    // unit stayed "active" for over an hour. `high` is a throttle for elastic
    // classes (page cache, children), not for the single process that must
    // serve. The parent `MemoryHigh=960M` still throttles the whole service,
    // and `max` remains the backstop.
    // `memory.high` is written as `max` (i.e. disabled) rather than skipped:
    // an existing hierarchy from an older binary still carries the 512M value,
    // and a restart must clear it, not inherit it.
    for (path, value) in [
        (
            root.join("main/memory.low"),
            (256u64 * 1024 * 1024).to_string(),
        ),
        (root.join("main/memory.high"), "max".to_string()),
        (
            root.join("main/memory.max"),
            (768u64 * 1024 * 1024).to_string(),
        ),
    ] {
        if std::fs::write(&path, value).is_err() {
            tracing::warn!("partition: write {} failed", path.display());
            leaf_failures += 1;
        }
    }
    if has_cpu {
        // 5:1 over the child classes. Only distributes when both are active.
        for (path, weight) in [
            (root.join("main/cpu.weight"), "500"),
            (kids.join("cpu.weight"), "100"),
        ] {
            if std::fs::write(&path, weight).is_err() {
                tracing::warn!("partition: write {} failed", path.display());
                leaf_failures += 1;
            }
        }
    } else {
        tracing::warn!(
            "partition: cpu controller unavailable — build contention will not be weighted"
        );
    }

    ocserve_store::cgroup::publish(root);
    log_budget(root, &kids, mb);
    if leaf_failures > 0 {
        return Partition::Degraded("leaf-cap-write-failed");
    }
    Partition::Ok
}

/// Remove empty `main` / `kids` subdirectories a nested ocserve left inside a
/// class leaf. Only empty directories are removed; anything holding processes
/// is left alone and reported.
fn sweep_stray_subdirs(leaf: &Path, name: &str) {
    for stray in ["main", "kids"] {
        let p = leaf.join(stray);
        if !p.is_dir() {
            continue;
        }
        match std::fs::remove_dir(&p) {
            Ok(()) => tracing::info!("partition: swept stray kids/{name}/{stray}"),
            Err(e) => tracing::debug!(
                "partition: kids/{name}/{stray} not swept ({e}) — likely still holds processes"
            ),
        }
    }
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

/// Say the budget out loud instead of implying it in a comment. The 2026-10-21
/// kill was caused by placement, but the arithmetic was structurally
/// over-committed: kids 700 + main 320 exceeds the 1024 parent before slab.
/// Reading the numbers from the kernel keeps this honest if a knob changes.
fn log_budget(root: &Path, kids: &Path, kids_mb: u64) {
    let read_mb = |p: &Path| {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|b| b / 1024 / 1024)
    };
    let parent_max = read_mb(&root.join("memory.max"));
    let parent_high = read_mb(&root.join("memory.high"));
    let main_low = read_mb(&root.join("main/memory.low")).unwrap_or(0);
    let main_max = read_mb(&root.join("main/memory.max")).unwrap_or(0);
    let kids_high = read_mb(&kids.join("memory.high")).unwrap_or(0);
    let leaves = ocserve_store::cgroup::LEAVES
        .iter()
        .filter(|n| kids.join(n).is_dir())
        .count();
    // Per-class caps are the control; the aggregate max is only the backstop.
    // A leaf cap is a ceiling, not a reservation, so the sum may exceed the
    // parent (limits are allowed to be over-committed, kernel cgroup-v2).
    // The PARENT max is the real ceiling: `main.max + kids.max` is deliberately
    // larger, and the parent `MemoryHigh` throttles across classes.
    let class_caps: Vec<String> = ocserve_store::cgroup::LEAVES
        .iter()
        .map(|n| {
            let p = ocserve_store::cgroup::leaf_policy(n);
            format!("{n}={}M/{}M", p.high_mb, p.max_mb)
        })
        .collect();
    match parent_max {
        Some(p) => {
            // UNRECLAIMABLE comparison: kids.max + main.low are anon+slab
            // commitments; page cache is charged to the parent but reclaimed
            // first. 64 MB is the working reserve for slab and page tables.
            const HEADROOM_MB: u64 = 64;
            let committed = kids_mb + main_low;
            if committed + HEADROOM_MB > p {
                tracing::warn!(
                    "memory budget tight: parent {p}M, kids.max {kids_mb}M + main.low \
                     {main_low}M leaves {}M for anon+slab (want {HEADROOM_MB}M) — \
                     each class is capped, high={}M/{kids_high}M, SRE §5.5",
                    p.saturating_sub(committed),
                    parent_high.unwrap_or(0)
                );
            } else {
                tracing::info!(
                    "memory budget: parent {p}M (high {}M), main {main_low}M low / {main_max}M max, \
                     kids {kids_mb}M (high {kids_high}M), {}M headroom; classes {}",
                    parent_high.unwrap_or(0),
                    p.saturating_sub(committed),
                    class_caps.join(" ")
                );
            }
        }
        None => tracing::info!(
            "memory budget: kids.max {kids_mb}M (high {kids_high}M), main.low {main_low}M, \
             {leaves} class leaves; classes {}",
            class_caps.join(" ")
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
                // Real delegated user trees expose cpu alongside memory.
                "cpuset cpu memory pids"
            } else {
                "cpuset pids"
            },
        )
        .unwrap();
        std::fs::write(root.join("cgroup.subtree_control"), "").unwrap();
        root
    }

    #[test]
    fn stray_leaf_subdirs_are_swept_but_occupied_ones_survive() {
        let root = fixture(true);
        assert_eq!(setup_in(&root, false), Partition::Ok);
        let leaf = root.join("kids/tools");
        // Simulate the nested-server leftovers: two EMPTY dirs and one that
        // still looks occupied (we cannot add processes to a fake cgroup, so
        // a file stands in for "not empty").
        std::fs::create_dir_all(leaf.join("main")).unwrap();
        std::fs::create_dir_all(leaf.join("kids")).unwrap();
        std::fs::write(leaf.join("kids/occupied"), "x").unwrap();
        sweep_stray_subdirs(&leaf, "tools");
        assert!(!leaf.join("main").exists(), "empty main/ swept");
        assert!(leaf.join("kids").exists(), "non-empty kids/ left alone");
        let _ = std::fs::remove_dir_all(&root);
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
            // Per-class caps are the containment primitive: without them every
            // leaf inherits `max` and a build starves the plugin host.
            let policy = ocserve_store::cgroup::leaf_policy(name);
            assert_eq!(
                std::fs::read_to_string(leaf.join("memory.max")).unwrap(),
                (policy.max_mb * 1024 * 1024).to_string(),
                "leaf {name} needs its own memory.max"
            );
            assert_eq!(
                std::fs::read_to_string(leaf.join("memory.high")).unwrap(),
                (policy.high_mb * 1024 * 1024).to_string(),
                "leaf {name} needs a throttle below max"
            );
            assert!(
                policy.high_mb < policy.max_mb,
                "leaf {name}: high must be below max"
            );
            assert_eq!(
                std::fs::read_to_string(leaf.join("cpu.weight")).unwrap(),
                policy.cpu_weight.to_string(),
                "leaf {name} needs a CPU weight"
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.join("main/memory.high")).unwrap(),
            "max",
            "main must NOT have an effective memory.high: a single non-reclaimable \
             process is throttled into a livelock rather than reclaimed (2026-10-24 hang)"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("main/memory.max")).unwrap(),
            (768u64 * 1024 * 1024).to_string(),
            "main needs its own hard backstop"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("main/cpu.weight")).unwrap(),
            "500",
            "the server must outweigh its children"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("kids/cpu.weight")).unwrap(),
            "100"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("kids/memory.high")).unwrap(),
            (640u64 * 1024 * 1024).to_string(),
            "kids must throttle before the hard cap"
        );
        // Aggregate must NOT carry oom.group: kids/ is a set of independent
        // services, so a build spiking must not take the browser host with it.
        assert!(
            !root.join("kids/memory.oom.group").exists(),
            "kids/ must not be an oom group"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("main/memory.low")).unwrap(),
            (256u64 * 1024 * 1024).to_string(),
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
        assert!(
            sc.contains("+cpu"),
            "subtree_control must enable cpu for weights: {sc}"
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
    fn partial_leaf_budget_is_degraded_not_ok() {
        let root = fixture(true);
        // Make kids/ a FILE so leaf creation fails after the aggregate caps
        // succeed; the point is that the result cannot be "ok".
        std::fs::create_dir_all(root.join("kids")).unwrap();
        std::fs::write(root.join("kids/tools"), "not a dir").unwrap();
        let p = setup_in(&root, false);
        assert_eq!(p, Partition::Degraded("leaf-cap-write-failed"));
        let _ = std::fs::remove_dir_all(&root);
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

    const ROOT: Option<&str> =
        Some("/user.slice/user-1000.slice/user@1000.service/app.slice/ocserve.service");

    #[test]
    fn enabled_requires_force_or_the_units_main_process() {
        // bare/test/harness (no env, no INVOCATION_ID) → NEVER restructures
        assert!(!enabled_from(None, None, None, 42, ROOT));
        // kill switch wins over everything
        assert!(!enabled_from(Some("0"), Some("id"), Some(42), 42, ROOT));
        // force works without systemd (manual experiments)
        assert!(enabled_from(Some("1"), None, None, 42, None));
        // systemd main process at the unit root → on
        assert!(enabled_from(None, Some("id"), Some(42), 42, ROOT));
        // junk env = unset (never panics, never guesses)
        assert!(!enabled_from(Some("junk"), None, Some(42), 42, ROOT));
        assert!(enabled_from(Some("junk"), Some("id"), Some(42), 42, ROOT));
    }

    /// Inherited INVOCATION_ID is not identity: a test server (or a nested
    /// `ocserve serve` started by a tool) carries the parent's exec pid.
    #[test]
    fn inherited_unit_identity_never_restructures() {
        assert!(
            !enabled_from(None, Some("inherited"), Some(1195), 9999, ROOT),
            "child of a unit must decline"
        );
        // absent SYSTEMD_EXEC_PID (systemd < 248): decline rather than guess
        assert!(!enabled_from(None, Some("inherited"), None, 9999, ROOT));
    }

    /// The nested-server bug: a tool child inherits INVOCATION_ID, so presence
    /// alone is not enough. Shape decides.
    #[test]
    fn nested_cgroup_never_restructures() {
        let unit = "/user.slice/u.service/app.slice/ocserve.service";
        for nested in [
            format!("{unit}/main"),
            format!("{unit}/kids/tools"),
            format!("{unit}/kids/tools/main"),
            format!("{unit}/kids/plugin"),
        ] {
            assert!(
                !enabled_from(None, Some("inherited-id"), Some(7), 7, Some(&nested)),
                "{nested} must decline the dance"
            );
        }
        assert!(enabled_from(
            None,
            Some("inherited-id"),
            Some(7),
            7,
            Some(unit)
        ));
        assert!(at_unit_root("/user.slice/u.service/app.slice/run-1.scope"));
        assert!(!at_unit_root(
            "/user.slice/u.service/app.slice/ocserve.service/kids"
        ));
        assert!(!at_unit_root(""));
    }
}
