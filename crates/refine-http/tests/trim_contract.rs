//! K-OOM trim contract (Tier-1.3): `trim_heap()` returns freed glibc pages
//! to the OS and is env-gated — `REFINE_TRIM=0` must make it a no-op so the
//! lab negative control (`oom_reload.py notrim`) keeps meaning what it says.
//!
//! Single test fn per file = no env race (integration test files run in
//! separate processes; see TESTING §1 flaky-test rules).

#[test]
fn trim_heap_respects_killswitch() {
    // default (no env): must actually call malloc_trim on this platform
    assert!(
        refine_http::watch::trim_heap(),
        "trim_heap must be active by default on linux-gnu"
    );

    // killswitch: REFINE_TRIM=0 → no-op, returns false
    // SAFETY: single-threaded test process; var restored below.
    unsafe { std::env::set_var("REFINE_TRIM", "0") };
    assert!(
        !refine_http::watch::trim_heap(),
        "REFINE_TRIM=0 must disable trim (lab negative control depends on it)"
    );

    // unrelated value: active again
    unsafe { std::env::set_var("REFINE_TRIM", "1") };
    assert!(
        refine_http::watch::trim_heap(),
        "REFINE_TRIM=1 stays active"
    );

    unsafe { std::env::remove_var("REFINE_TRIM") };
    assert!(
        refine_http::watch::trim_heap(),
        "env removed → active again"
    );
}
