//! Boot must not die on transient write-lock contention during post-migration
//! statistics maintenance (PERF-10X stmt/pragma audit, found by the load
//! harness 2026-10-07: a stale fixture wipe left a second connection holding
//! the write lock, `PRAGMA optimize` returned SQLITE_BUSY, and
//! `ocserve serve` exited 1 → "ocserve never healthy").
//!
//! Contract: (a) boot survives contention and still collects stats when it
//! wins, (b) non-contention errors stay loud, (c) the retry is bounded.

use std::time::Duration;

/// Hold an EXCLUSIVE lock on the database for `hold`, then release.
fn hold_write_lock(db: &std::path::Path, hold: Duration) {
    let blocker = rusqlite::Connection::open(db).unwrap();
    blocker
        .execute_batch("BEGIN EXCLUSIVE; CREATE TABLE IF NOT EXISTS _lock_probe(x);")
        .unwrap();
    std::thread::sleep(hold);
    let _ = blocker.execute_batch("ROLLBACK;");
}

#[test]
fn boot_survives_a_contended_optimize() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    {
        let conn = ocserve_store::pragma::create_new(&db).unwrap();
        ocserve_store::schema::migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                                  cost, time_created, time_updated)
             VALUES ('ses_a','p','/','p','s','t','1',0.0,1,1);",
        )
        .unwrap();
    }

    // blocker holds the write lock for 400ms; our retry budget is 5 attempts
    // with 100/200/300/400 ms backoff = 1000 ms total
    let db2 = db.clone();
    let blocker = std::thread::spawn(move || hold_write_lock(&db2, Duration::from_millis(400)));

    // a short settle so the blocker wins the race
    std::thread::sleep(Duration::from_millis(60));

    let conn = ocserve_store::pragma::create_new(&db).unwrap();
    let t0 = std::time::Instant::now();
    let res = ocserve_store::schema::migrate(&conn);
    let elapsed = t0.elapsed();
    let _ = blocker.join();

    assert!(
        res.is_ok(),
        "boot must survive transient write-lock contention, got: {:?}",
        res.err()
    );
    // it must not have hung either: the retry budget is bounded
    assert!(
        elapsed < Duration::from_secs(5),
        "retry must be bounded, took {elapsed:?}"
    );
}

#[test]
fn stats_are_collected_when_the_lock_is_free() {
    // the positive half: with no contention, migrate must leave planner stats
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    let conn = ocserve_store::pragma::create_new(&db).unwrap();
    ocserve_store::schema::migrate(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                              cost, time_created, time_updated)
         VALUES ('ses_a','p','/','p','s','t','1',0.0,1,1);
         INSERT INTO msg (id, session_id, role, seq, time_created, info)
         VALUES ('m1','ses_a','user',1,100,'{}');",
    )
    .unwrap();
    ocserve_store::schema::migrate(&conn).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='sqlite_stat1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "uncontended boot must leave planner stats behind");
}

/// The FTS segment merge was REMOVED from `post_maintenance` (2026-10-08:
/// it measured 175,796 ms on the live 1.6 GB index with serve blocked, vs a
/// 6 ms `PRAGMA optimize`). This test guards that removal: a missing
/// `part_search_fts` must no longer block boot, because nothing on the boot
/// path may touch it.
///
/// Non-contention loudness (the property this test used to check via a
/// missing FTS table) now lives where it can be tested deterministically:
/// `schema::retry_policy_tests::non_contention_errors_propagate_on_the_first_try`.
#[test]
fn migrate_succeeds_without_the_fts_table() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    {
        let conn = ocserve_store::pragma::create_new(&db).unwrap();
        ocserve_store::schema::migrate(&conn).unwrap();
        conn.execute_batch("DROP TABLE part_search_fts;").unwrap();
    }
    let conn = ocserve_store::pragma::create_new(&db).unwrap();
    let res = ocserve_store::schema::migrate(&conn);
    assert!(
        res.is_ok(),
        "boot must not depend on the FTS segment merge, got: {:?}",
        res.err()
    );
}
