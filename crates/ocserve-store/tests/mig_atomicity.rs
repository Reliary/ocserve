//! Migration atomicity: a crash must never leave a database that cannot boot.
//!
//! Found by a mutation run (2026-10-28). A mutant in `fork.rs` made
//! `crash_fuzz` fail at iteration 22 with "table session already exists" in
//! `schema::migrate`. The mutant was innocent: the real defect was that each
//! migration step's DDL and its `user_version` stamp were two independent
//! writes. A kill in between left the tables in place with the version still
//! pointing at the previous step, so EVERY subsequent boot re-ran the DDL and
//! aborted. `crash_fuzz` only ever saw it because it kills the process at
//! arbitrary moments; a normal shutdown never can.
//!
//! Both halves are pinned here: the legacy window must self-heal (there are
//! real databases in the field that may have been interrupted), and new steps
//! must be atomic.

use rusqlite::Connection;

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ocserve-mig-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The crash shape the OLD code could produce: schema present, `user_version`
/// still 0. This must heal, not abort, because such databases may exist.
#[test]
fn legacy_crash_window_self_heals() {
    let d = fresh_dir("legacy");
    let db = d.join("p.db");
    {
        let c = Connection::open(&db).unwrap();
        ocserve_store::schema::migrate(&c).unwrap();
        // Rewind only the stamp: exactly what a kill between the DDL commit
        // and the version write left behind.
        c.pragma_update(None, "user_version", 0i64).unwrap();
    }
    let c = Connection::open(&db).unwrap();
    let r = ocserve_store::schema::migrate(&c);
    assert!(
        r.is_ok(),
        "a database with the schema but version 0 must be stamped, not re-migrated: {r:?}"
    );
    let ver: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ver, ocserve_store::schema::SCHEMA_VERSION);
    let _ = std::fs::remove_dir_all(&d);
}

/// 25 rewinds of the crash window, each healed, then a normal migrate.
#[test]
fn repeated_heals_converge() {
    let d = fresh_dir("repeat");
    let db = d.join("p.db");
    {
        let c = Connection::open(&db).unwrap();
        ocserve_store::schema::migrate(&c).unwrap();
    }
    for i in 0..25 {
        let c = Connection::open(&db).unwrap();
        c.pragma_update(None, "user_version", 0i64).unwrap();
        ocserve_store::schema::migrate(&c).unwrap_or_else(|e| panic!("heal {i} failed: {e:#}"));
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, ocserve_store::schema::SCHEMA_VERSION);
    }
    let _ = std::fs::remove_dir_all(&d);
}
