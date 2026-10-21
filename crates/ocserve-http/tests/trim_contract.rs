//! K-OOM trim contract (Tier-1.3): `trim_heap()` returns freed glibc pages
//! to the OS and is env-gated — `OCSERVE_TRIM=0` must make it a no-op so the
//! lab negative control (`oom_reload.py notrim`) keeps meaning what it says.
//!
//! Single test fn per file = no env race (integration test files run in
//! separate processes; see TESTING §1 flaky-test rules).

/// glibc has `malloc_trim`; musl builds use mimalloc, whose `mi_collect` is
/// the equivalent; everywhere else the trim is a documented no-op.
fn purge_available() -> bool {
    cfg!(all(target_os = "linux", target_env = "gnu"))
        || cfg!(all(target_os = "linux", target_env = "musl"))
}

#[test]
fn trim_heap_respects_killswitch() {
    // Default (no env): reports whether a real purge ran on this target.
    let trimmed = ocserve_http::watch::trim_heap();
    assert_eq!(
        trimmed,
        purge_available(),
        "trim_heap must report whether a real purge ran on this target"
    );

    // killswitch: OCSERVE_TRIM=0 → no-op, returns false
    // SAFETY: single-threaded test process; var restored below.
    unsafe { std::env::set_var("OCSERVE_TRIM", "0") };
    assert!(
        !ocserve_http::watch::trim_heap(),
        "OCSERVE_TRIM=0 must disable trim (lab negative control depends on it)"
    );

    // unrelated value: active again (same platform dependence as above)
    unsafe { std::env::set_var("OCSERVE_TRIM", "1") };
    let trimmed = ocserve_http::watch::trim_heap();
    assert_eq!(trimmed, purge_available(), "OCSERVE_TRIM=1 stays active");

    unsafe { std::env::remove_var("OCSERVE_TRIM") };
    let trimmed = ocserve_http::watch::trim_heap();
    assert_eq!(trimmed, purge_available(), "env removed → active again");
}
