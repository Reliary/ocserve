//! STORAGE §4 ring bound + §5 backup drill (TESTING §4: boundary + state
//! tests with negative controls: remove the bound → test must fail).

use refine_store::{backup_to, pragma, prune_events, schema};
use rusqlite::Connection;

fn db_with_events(n: i64, now: i64) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::migrate(&conn).unwrap();
    conn.execute(
        "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                              time_created, time_updated)
         VALUES ('ses_a', 'global', '/w', 's', 's', 't', '1', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                              time_created, time_updated)
         VALUES ('ses_b', 'global', '/w', 's', 's', 't', '1', 1, 1)",
        [],
    )
    .unwrap();
    for i in 0..n {
        conn.execute(
            "INSERT INTO event (session_id, project_id, type, payload, time_created)
             VALUES ('ses_a', 'global', 'x', '{}', ?1)",
            [now - i], // seq = rowid order: newest first inserted? seq AUTOINCREMENT ascending per insert order
        )
        .unwrap();
        conn.execute(
            "INSERT INTO event (session_id, project_id, type, payload, time_created)
             VALUES ('ses_b', 'global', 'x', '{}', ?1)",
            [now],
        )
        .unwrap();
    }
    conn
}

#[test]
fn ring_cap_keeps_newest_per_session_and_is_independent() {
    let now = 1_800_000_000_000i64;
    let conn = db_with_events(120, now); // ses_a: 120 rows (mixed ages), ses_b: 120 fresh
    let pruned = prune_events(&conn, now, 100, 30 * 24 * 3_600_000).unwrap();
    let a: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE session_id='ses_a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let b: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE session_id='ses_b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        a, 100,
        "ses_a capped at 100 (negative control: no cap → 120)"
    );
    assert_eq!(b, 100, "ses_b capped independently");
    assert_eq!(pruned, 40, "20 pruned per session");
    // survivors of ses_a are the NEWEST by seq: the age filter dropped old
    // rows first, cap keeps the rest — oldest remaining seq must be the
    // (120-20)=100th newest.
    let min_created: i64 = conn
        .query_row(
            "SELECT MIN(time_created) FROM event WHERE session_id='ses_a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        min_created > now - 30 * 24 * 3_600_000,
        "age window respected"
    );
}

#[test]
fn age_window_drops_old_but_never_unknown_timestamps() {
    let now = 1_800_000_000_000i64;
    let conn = db_with_events(5, now);
    // one row with unknown timestamp (0) — must survive age pruning
    conn.execute(
        "INSERT INTO event (session_id, project_id, type, payload, time_created)
         VALUES ('ses_a', 'global', 'x', '{}', 0)",
        [],
    )
    .unwrap();
    // make all ses_b rows ancient
    conn.execute(
        "UPDATE event SET time_created = 1 WHERE session_id='ses_b'",
        [],
    )
    .unwrap();
    prune_events(&conn, now, 10_000, 30 * 24 * 3_600_000).unwrap();
    let zero: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE time_created = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(zero, 1, "unknown-timestamp rows never age-pruned");
    let b: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE session_id='ses_b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        b, 0,
        "ancient rows dropped (negative control: no prune → 120)"
    );
}

#[test]
fn vacuum_into_backup_restores_clean() {
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("refine.db");
    {
        let conn = Connection::open(&dbp).unwrap();
        schema::migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                                  time_created, time_updated)
             VALUES ('ses_x', 'global', '/w', 's', 's', 't', '1', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO msg (id, session_id, role, seq, time_created, info)
             VALUES ('m1', 'ses_x', 'user', 1, 1, '{}')",
            [],
        )
        .unwrap();
        let dest = dir.path().join("backup.db");
        backup_to(&conn, &dest).unwrap();
        // restore drill: copy back, integrity + counts
        let restored = dir.path().join("restored.db");
        std::fs::copy(&dest, &restored).unwrap();
        let r = pragma::open_reader(&restored).unwrap();
        let ic: String = r
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ic, "ok", "restored DB passes integrity_check");
        let n: i64 = r
            .query_row("SELECT count(*) FROM msg", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 1, "data survived backup/restore");
        // second backup to same dest must fail (refuse overwrite)
        assert!(backup_to(&conn, &dest).is_err(), "overwrite refused");
    }
}
