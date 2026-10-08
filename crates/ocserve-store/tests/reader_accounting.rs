//! K-EFFICIENCY reader-open accounting — PROCESS-ISOLATED on purpose (TESTING §1.6).
//!
//! The assertions are exact deltas against the process-global `READER_OPENS`
//! counter. As an in-crate unit test they raced sibling tests that also open
//! readers (observed fail→pass on identical code, 2026-10-05: flaky test =
//! defect). This integration binary hosts this single test, so the counter
//! has no concurrent writers, and the crate is compiled WITHOUT `cfg(test)`
//! here — the production code path is what gets asserted (mutation-kill
//! parity with the old in-src copy: removing the `fetch_add` turns this red).

use ocserve_store::pragma::{create_new, open_reader, reader_opens};

/// K-EFFICIENCY: same-path reopen on one thread returns the PARKED
/// connection (fresh-open accounting does not grow); a nested open
/// while checked out falls back to a transient (never aliases).
#[test]
fn reader_reuses_parked_connection_per_thread() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    {
        let conn = create_new(&db).unwrap();
        drop(conn);
    }
    let before = reader_opens();
    {
        let a = open_reader(&db).unwrap();
        assert_eq!(reader_opens(), before + 1, "first open is fresh");
        drop(a);
        let b = open_reader(&db).unwrap();
        assert_eq!(
            reader_opens(),
            before + 1,
            "same-path reopen must CHECK OUT the parked conn (no new open)"
        );
        // reentrant open while checked out → fresh transient (no aliasing)
        let inner = open_reader(&db).unwrap();
        assert_eq!(reader_opens(), before + 2, "nested open falls back fresh");
        drop(inner);
        drop(b);
    }
    let c = open_reader(&db).unwrap();
    assert_eq!(
        reader_opens(),
        before + 2,
        "parked conn still reusable after the scope"
    );
    drop(c);
}
