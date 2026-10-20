//! Event sequence allocation must be atomic with the append.
//!
//! `emit_event` (the durable choke used by HTTP handlers and v2 admission)
//! returned `SELECT MAX(seq) WHERE session_id` from a SEPARATE reader after
//! `Writer::write` committed. Two emitters on one session can interleave:
//!
//!   T1 append commits at seq 10
//!   T2 append commits at seq 11
//!   T1 MAX -> 11        T2 MAX -> 11        <- duplicate
//!
//! The comment justified the read-back with "the writer serializes all
//! appends". That is true of the INSERT and false of the window after it.
//! Upstream allocates inside the transaction (`core/src/event.ts:318`,
//! `EventSequenceTable` with `onConflictDoUpdate`).
//!
//! Blast radius: the durable `seq` feeds the v1 sync-twin `seq` and the inbox
//! `enqueued_seq` (which `inbox_list` orders by). The v2 log watermark uses
//! the table's AUTOINCREMENT rowid, which stays monotonic, so it was never
//! affected.
//!
//! CONTROL (proven red before the fix): with the read-back restored, this test
//! reports duplicate seq values under concurrent emits.

use ocserve_store::{pragma, schema, writer};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn migrated() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = writer::db_path(dir.path());
    let conn = pragma::open_writer(&db).unwrap();
    schema::migrate(&conn).unwrap();
    (dir, db)
}

fn seed_session(db: &std::path::Path) {
    let conn = pragma::open_writer(db).unwrap();
    conn.execute(
        "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) \
         VALUES ('ses_seq', 'global', '/w', 's', 's', 't', '1', 1, 1)",
        [],
    )
    .unwrap();
}

#[test]
fn concurrent_emits_allocate_distinct_monotonic_seqs() {
    let (_dir, db) = migrated();
    seed_session(&db);

    const THREADS: usize = 8;
    const PER_THREAD: usize = 25;

    let w = Arc::new(writer::Writer::spawn(db.clone()).unwrap());
    let next_call = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let w = Arc::clone(&w);
        let next_call = Arc::clone(&next_call);
        handles.push(std::thread::spawn(move || {
            let mut seen = Vec::with_capacity(PER_THREAD);
            for _ in 0..PER_THREAD {
                let n = next_call.fetch_add(1, Ordering::Relaxed);
                let (seq, id) = ocserve_store::emit_event(
                    &w,
                    Some("ses_seq"),
                    "session.inbox.enqueued",
                    &serde_json::json!({"n": n}),
                )
                .expect("emit");
                seen.push((seq, id));
            }
            seen
        }));
    }
    let mut all: Vec<(i64, String)> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("thread"))
        .collect();

    let total = (THREADS * PER_THREAD) as i64;
    // Duplicates are the bug. Sort so the failure message is readable.
    all.sort_by_key(|(seq, _)| *seq);
    let mut dups = Vec::new();
    for pair in all.windows(2) {
        if pair[0].0 == pair[1].0 {
            dups.push(pair[0].0);
        }
    }
    assert!(
        dups.is_empty(),
        "duplicate event seq allocated under concurrency: {:?} (of {} emits)",
        dups,
        total
    );

    // And every seq must be distinct: 1..=total with no holes. Holes are not
    // the bug under test (other writers could interleave), but duplicates are.
    let distinct = all
        .iter()
        .map(|(s, _)| *s)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        distinct.len(),
        all.len(),
        "seq collision: {} distinct of {}",
        distinct.len(),
        all.len()
    );

    // The stable id must be derived from the same allocation, not a separate
    // clock read (a client that keys by id and watermarks by seq would drift).
    for (seq, id) in &all {
        assert_eq!(id, &format!("evt_{seq}"), "id/seq disagree");
    }
}

/// A coalesced writer transaction must still report PER-CALLER row counts.
///
/// The writer coalesces up to 50 ms of batches into one `BEGIN IMMEDIATE`
/// transaction. Before this test, every ack in that transaction received the
/// coalesced total, so two concurrent `INSERT OR IGNORE` callers both observed
/// `n = 2` and each concluded "someone else inserted it". `inbox::admit`
/// returns `Ok(n == 1)` from exactly that count, so concurrent admissions could
/// both report "already present" and neither would be delivered.
///
/// CONTROL (proven red before the per-batch fix): the assertion below saw
/// counts > 1 for single-row inserts.
#[test]
fn concurrent_admits_report_their_own_row_count() {
    let (_dir, db) = migrated();
    seed_session(&db);
    let w = Arc::new(writer::Writer::spawn(db.clone()).unwrap());

    const N: usize = 16;
    let mut handles = Vec::new();
    for i in 0..N {
        let w = Arc::clone(&w);
        handles.push(std::thread::spawn(move || {
            // Each thread inserts its own row: every one must report n == 1.
            let n = w
                .write(vec![ocserve_store::WriteOp::Sql {
                    sql: "INSERT OR IGNORE INTO session_inbox \
                          (id, session_id, type, payload, delivery, enqueued_seq, time_created) \
                          VALUES (?1, 'ses_seq', 'user', '{}', 'queue', ?2, 1)"
                        .into(),
                    params: vec![format!("inbox_{i}").into(), (i as i64).into()],
                }])
                .expect("admit");
            (i, n)
        }));
    }
    let results: Vec<(usize, usize)> = handles
        .into_iter()
        .map(|h| h.join().expect("thread"))
        .collect();
    let wrong: Vec<_> = results.iter().filter(|(_, n)| *n != 1).collect();
    assert!(
        wrong.is_empty(),
        "single-row inserts reported coalesced counts: {:?}",
        wrong
    );
    let count: i64 = {
        let conn = pragma::open_reader(&db).unwrap();
        conn.query_row("SELECT count(*) FROM session_inbox", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count, N as i64, "every admitted row must persist");
}
