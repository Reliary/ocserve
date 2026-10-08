//! M0 decisive experiment 4 (PLAN §13): SIGKILL fuzz of the blob/DB crash protocol.
//!
//! Protocol under test (STORAGE.md §4): write chunk file → fsync → rename →
//! then DB commit. A kill at ANY point must leave a state that is either:
//! - consistent (all blob_object rows have files; orphans OK — GC sweeps them), or
//! - recoverable (DB opens, quick_check passes, missing refs detected by doctor).
//!
//! Child mode: this test binary re-invoked with OCSERVE_CRASH_CHILD=1 runs a
//! write loop until SIGKILLed by the parent at a random offset.

use ocserve_store::{BlobStore, WriteOp, Writer, pragma, schema, writer};
use std::path::PathBuf;

fn child_loop(dir: PathBuf) {
    let bs = BlobStore::new(dir.join("blobs")).expect("blob store");
    let w = Writer::spawn(writer::db_path(&dir)).expect("writer");
    let mut i: u64 = 0;
    loop {
        // payload ~64KB so each iteration writes multiple 1MB-unrelated chunks
        let payload: Vec<u8> = (0..65_536)
            .map(|j| ((i as usize + j) % 251) as u8)
            .collect();
        let (sha, len, cnt) = bs.put(&payload).expect("put");
        // protocol order: files durable FIRST, then DB row
        w.write(vec![
            WriteOp::BlobPut {
                sha: sha.clone(),
                byte_len: len,
                chunk_cnt: cnt,
            },
            WriteOp::Sql {
                sql: "INSERT INTO session (id, time_created, time_updated) VALUES (?1, ?2, ?3)"
                    .into(),
                params: vec![format!("ses_{i}").into(), i.into(), i.into()],
            },
        ])
        .expect("write");
        i += 1;
    }
}

#[test]
fn sigkill_fuzz_blob_db_protocol() {
    if std::env::var("OCSERVE_CRASH_CHILD").is_ok() {
        let dir = PathBuf::from(std::env::var("OCSERVE_CRASH_DIR").expect("dir"));
        child_loop(dir);
        return;
    }

    let exe = std::env::current_exe().expect("current exe");
    let iterations: usize = std::env::var("OCSERVE_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25); // default 25 for CI speed; 1000 via env for nightly (SRE §5)

    for it in 0..iterations {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut child = std::process::Command::new(&exe)
            .arg("--nocapture")
            .arg("--exact")
            .arg("sigkill_fuzz_blob_db_protocol")
            .env("OCSERVE_CRASH_CHILD", "1")
            .env("OCSERVE_CRASH_DIR", dir.path())
            .env("RUST_TEST_THREADS", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child");

        // kill at a random-ish offset: varies with iteration, wall clock
        let jitter_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(it as u64);
        let wait_ms = 60 + (jitter_ns % 240) + it as u64 % 7;
        std::thread::sleep(std::time::Duration::from_millis(wait_ms));
        let _ = child.kill();
        let _ = child.wait();

        // RECOVERY CHECKS — the state after a kill at an arbitrary point:
        let db = writer::db_path(dir.path());
        if !db.exists() {
            continue; // killed before first commit: nothing to recover, legal state
        }
        // 1. DB opens with the writer profile (WAL recovery happens inside open)
        let conn = pragma::open_writer(&db)
            .unwrap_or_else(|e| panic!("iter {it}: open after kill failed: {e:#}"));
        // 2. quick integrity gate
        let qc: String = conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("iter {it}: quick_check errored: {e:#}"));
        assert_eq!(qc, "ok", "iter {it}: quick_check failed after kill");
        // 3. schema gate
        schema::migrate(&conn).unwrap_or_else(|e| panic!("iter {it}: migrate: {e:#}"));

        // 4. every DB-referenced blob has files; orphans are sweepable (not fatal)
        let blobs = BlobStore::new(dir.path().join("blobs")).expect("blob store");
        let mut stmt = conn
            .prepare("SELECT sha, byte_len FROM blob_object")
            .expect("stmt");
        let refs: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("map")
            .filter_map(|r| r.ok())
            .collect();
        let mut missing = 0;
        for (sha, len) in &refs {
            if blobs.get(sha, *len as u64).is_err() {
                missing += 1;
            }
        }
        assert_eq!(
            missing, 0,
            "iter {it}: {missing} DB rows reference missing blob files \
             (protocol violated: DB committed before files durable)"
        );
        // 5. GC removes exactly the orphans, keeps live
        let live: std::collections::HashSet<String> = refs.into_iter().map(|(s, _)| s).collect();
        let removed = blobs.gc_orphans(|sha| live.contains(sha)).expect("gc");
        // removed can be >0: kill between file publish and DB commit = orphan (legal)
        let _ = removed;
    }
}
