//! M0 decisive experiment (PLAN §13): 100k-row fixture with the production
//! cache profile; asserts the aggregate-cache budget arithmetic (MEMORY §1)
//! and a p95-latency smoke on the hot list query at this scale.
//!
//! Full MemoryMax=300M enforcement lives in the soak job (systemd); here we
//! verify the *declared* budget and that the hot queries stay indexed.

use ocserve_store::{WriteOp, Writer, pragma, writer};
use std::time::Instant;

#[test]
fn cache_budget_and_100k_row_latency() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = writer::db_path(dir.path());

    // --- declared budget arithmetic (MEMORY §1: aggregate page cache ≤ 32 MB) ---
    let writer_kb: i64 = pragma::WRITER_CACHE_KB.abs();
    let reader_kb: i64 = pragma::READER_CACHE_KB.abs();
    let reader_pool: i64 = 4; // STORAGE §2
    let aggregate_kb = writer_kb + reader_kb * reader_pool;
    assert!(
        aggregate_kb <= 32 * 1024,
        "aggregate cache budget {aggregate_kb} KiB exceeds 32 MiB cap"
    );

    // --- writer uses exactly the declared profile ---
    let w = Writer::spawn(db.clone()).expect("writer");
    {
        let c = pragma::open_writer(&db).expect("open");
        let cs: i64 = c
            .query_row("PRAGMA cache_size", [], |r| r.get(0))
            .expect("cache");
        assert_eq!(cs, pragma::WRITER_CACHE_KB);
        let ms: i64 = c
            .query_row("PRAGMA mmap_size", [], |r| r.get(0))
            .expect("mmap");
        assert_eq!(ms, 0, "mmap must stay off (budget + CIDR 2022)");
    }

    // --- 100k sessions, batched through the writer ---
    let t_insert = Instant::now();
    const N: usize = 100_000;
    let batch: Vec<WriteOp> = (0..N)
        .map(|i| WriteOp::Sql {
            sql: "INSERT INTO session (id, time_created, time_updated, title) VALUES (?1, ?2, ?3, ?4)"
                .into(),
            params: vec![
                format!("ses_{i:08}").into(),
                (i as i64).into(),
                (i as i64 * 7).into(),
                format!("session number {i} with a searchable title").into(),
            ],
        })
        .collect();
    // chunk to keep individual transactions bounded (≤50ms rule, STORAGE §3)
    for chunk in batch.chunks(2_000) {
        w.write(chunk.to_vec()).expect("batch write");
    }
    let insert_ms = t_insert.elapsed().as_millis();

    // --- hot query latency: session list (the M1 P0 route) ---
    let r = pragma::open_reader(&db).expect("reader");
    let mut best = std::time::Duration::ZERO;
    for _ in 0..20 {
        let t = Instant::now();
        let n: i64 = r
            .query_row(
                "SELECT count(*) FROM (SELECT id FROM session ORDER BY time_updated DESC LIMIT 50)",
                [],
                |x| x.get(0),
            )
            .expect("list query");
        assert_eq!(n, 50);
        best = best.max(t.elapsed());
    }
    let p95ish_ms = best.as_millis();

    // Gate (PLAN §7): session list p95 < 50 ms at 100k rows. Generous CI headroom
    // (DEBUG build, shared runner): fail at 500ms so a regression to SCAN shows,
    // exact 50ms gate runs on the release build in the bench job.
    assert!(
        p95ish_ms < 500,
        "list query at 100k rows took {p95ish_ms}ms — query plan regressed?"
    );

    // --- EXPLAIN audit: no full-table scan for the list query (STORAGE §7.1) ---
    let plan: String = r
        .query_row(
            "EXPLAIN QUERY PLAN SELECT id FROM session ORDER BY time_updated DESC LIMIT 50",
            [],
            |x| x.get(3),
        )
        .expect("eqp");
    assert!(
        plan.contains("idx_session_updated"),
        "list query lost its index, plan: {plan}"
    );
    // SQLite EQP: "SCAN session" (bare) = full table scan;
    // "SCAN session USING INDEX idx_..." = index scan (what we want).
    let full_scan = plan
        .split("SCAN session")
        .nth(1)
        .map(|rest| !rest.starts_with(" USING INDEX"))
        .unwrap_or(false);
    assert!(!full_scan, "full table scan detected: {plan}");

    println!(
        "M0-FIXTURE: {N} rows inserted in {insert_ms}ms; worst-of-20 list {p95ish_ms}ms; plan OK"
    );
}
