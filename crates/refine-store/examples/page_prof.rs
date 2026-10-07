//! Phase I profiler scenario (bench/perf/LEAN-PLAN.md I1): drive the REAL
//! production read paths against a real refine.db so bytehound attributes
//! allocations to actual code, not a synthetic micro-loop.
//!
//! Reproduces the mix the load harness actually issues, against a real deep
//! session:
//!   1. page of 50 messages, full JSON assembly (for_each_message_json)
//!   2. session list wire bytes
//!   3. session_exists / page_messages (SQL-only, for attribution contrast)
//!   4. search (fts + like paths)
//!
//! Env: PROFILE_DB (required), PROFILE_DEEP (session id; else auto-selects
//!      the biggest session and ASSERTS it is non-empty), PROFILE_ITERS.
//! Output goes to /dev/null-ish (a sink that keeps the work alive but
//! doesn't retain bytes) so the allocator profile reflects the real path.
//!
//! Run under: LD_PRELOAD=libbytehound.so refine-page-prof
//! Then:     bytehound analyze --symbols ... -o top-alloc.rhai
use std::io::Write;

fn main() -> anyhow::Result<()> {
    let db = std::env::var("PROFILE_DB").expect("PROFILE_DB=<refine.db>");
    let dbp = std::path::PathBuf::from(&db);
    let iters: usize = std::env::var("PROFILE_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);

    let deep = match std::env::var("PROFILE_DEEP") {
        Ok(s) => s,
        Err(_) => {
            let conn = refine_store::pragma::open_reader(&dbp)?;
            let sid: String = conn.query_row(
                "SELECT session_id FROM msg GROUP BY session_id ORDER BY count(*) DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?;
            sid
        }
    };

    // same gate as sqlite_tune_bench: never profile an empty session
    {
        let conn = refine_store::pragma::open_reader(&dbp)?;
        let n: i64 =
            conn.query_row("SELECT count(*) FROM msg WHERE session_id = ?1", [&deep], |r| {
                r.get(0)
            })?;
        anyhow::ensure!(n > 0, "PROFILE_DEEP={deep} has 0 messages — refusing");
        eprintln!("profile: deep={deep} msgs={n} iters={iters}");
    }

    // warmup outside the measured window is impossible with an external
    // profiler (it counts everything), so keep the ratio small: 20 warm
    // iterations then N measured. Documented, not hidden.
    let mut sink = std::io::BufWriter::new(std::io::sink());
    for i in 0..(iters + 20) {
        if i == 20 {
            eprintln!("profile: warmup done, {iters} measured iterations");
        }
        // 1. full page assembly (the dominant route)
        let (rows, _, _) = refine_store::page_messages(&dbp, &deep, 50, None)?;
        let mut bytes = 0usize;
        refine_store::for_each_message_json(
            &dbp,
            &deep,
            refine_store::MessageWalk::Window(rows),
            |chunk| {
                bytes += chunk.len();
                Ok(())
            },
        )?;
        // 2. session list wire bytes
        let list = refine_store::load_sessions_wire_bytes(&dbp)?;
        bytes += list.len();
        // 3. SQL-only contrast
        let exists = refine_store::session_exists(&dbp, &deep)?;
        anyhow::ensure!(exists, "session_exists false for {deep}");
        // 4. search
        let (hits, _) = refine_store::search_parts(&dbp, "the", None, 50, 0)?;
        bytes += hits.len() * 64;
        let _ = write!(sink, "{bytes}");
    }
    Ok(())
}