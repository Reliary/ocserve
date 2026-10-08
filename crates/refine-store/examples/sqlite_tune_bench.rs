//! SQLite tuning micro-bench (PERF-10X follow-up): runs the production
//! hot-path functions against a REAL refine.db and prints per-query
//! p50/p95, so mmap_size / page_size / prepare_cached changes are A/B
//! decisions, not opinions. READ-ONLY unless TUNE_VACUUM=1 (page_size
//! rebuild requires a scratch COPY — never run on a live db).
//!
//! Usage: sqlite_tune_bench <refine.db> [--reps N] [--deep SID]
//! Env:  REFINE_SEARCH_MEMO=0 REFINE_LIST_MEMO=0  (cold paths)
//!       TUNE_MMAP=268435456   (mmap override after open)
//!       TUNE_TAG=name          (label printed with results)
use std::time::Instant;

fn pcts(mut v: Vec<u128>) -> (u128, u128, u128) {
    v.sort_unstable();
    let n = v.len();
    let at = |q: f64| v[((n as f64) * q) as usize].min(v[n - 1]);
    (at(0.5), at(0.95), v[n - 1])
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let db = args.next().expect("db path");
    let mut reps: usize = 1000;
    let mut deep = String::new();
    let mut it = args.collect::<Vec<_>>().into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--reps" => reps = it.next().unwrap().parse()?,
            "--deep" => deep = it.next().unwrap(),
            _ => {}
        }
    }
    let dbp = std::path::PathBuf::from(&db);
    if std::env::var("TUNE_VACUUM").ok().as_deref() == Some("1") {
        // scratch-copy page_size rebuild (VACUUM rewrites every page)
        let conn = rusqlite::Connection::open(&dbp)?;
        let target: i64 = std::env::var("TUNE_PAGE_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8192);
        let t = Instant::now();
        conn.pragma_update(None, "page_size", target)?;
        conn.execute_batch("VACUUM")?;
        println!(
            "TUNE[{tag}] vacuum page_size={target} took {ms} ms",
            tag = std::env::var("TUNE_TAG").unwrap_or_default(),
            ms = t.elapsed().as_millis()
        );
        return Ok(());
    }
    if deep.is_empty() {
        // pick the session with the most messages
        let conn = rusqlite::Connection::open_with_flags(
            &dbp,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        // MUST be session_id, not id — `SELECT id … GROUP BY session_id`
        // returns a MESSAGE id, so every session-scoped query then measured
        // an empty session (found 2026-10-07: 32,341-msg session vs a
        // phantom `msg_…`). Negative control below proves the fix.
        deep = conn.query_row(
            "SELECT session_id FROM msg GROUP BY session_id ORDER BY count(*) DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )?;
    }
    let tag = std::env::var("TUNE_TAG").unwrap_or_else(|_| "base".into());
    // reopen through our profile (reader pragmas) then apply mmap override
    let r = refine_store::pragma::open_reader(&dbp)?;
    if let Some(b) = std::env::var("TUNE_MMAP")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
    {
        r.pragma_update(None, "mmap_size", b)?;
    }
    let st: i64 = r
        .query_row("PRAGMA mmap_size", [], |x| x.get(0))
        .unwrap_or(-1);
    let stat1: i64 = r
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='sqlite_stat1'",
            [],
            |x| x.get(0),
        )
        .unwrap();
    let stat4: i64 = r
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='sqlite_stat4'",
            [],
            |x| x.get(0),
        )
        .unwrap_or(0);
    // Hard gate: benchmarking an EMPTY session produces plausible-looking
    // single-digit-microsecond numbers that measure nothing — the 2026-10-07
    // bug class (the auto-selected "deep session" was a message id, so every
    // session-scoped query hit 0 rows). Refuse rather than publish them.
    let deep_msgs: i64 = r.query_row(
        "SELECT count(*) FROM msg WHERE session_id = ?1",
        [&deep],
        |x| x.get(0),
    )?;
    let deep_parts: i64 = r.query_row(
        "SELECT count(*) FROM msg_part WHERE session_id = ?1",
        [&deep],
        |x| x.get(0),
    )?;
    assert!(
        deep_msgs > 0,
        "deep={deep} has 0 messages — refusing to benchmark an empty session \
         (id-vs-session_id bug class, 2026-10-07)"
    );
    println!(
        "TUNE[{tag}] db={db} deep={deep} msgs={deep_msgs} parts={deep_parts} \
         reps={reps} mmap={st} stat1={stat1} stat4={stat4}"
    );

    // warmup (page-cache + parked reader + any statement cache)
    for _ in 0..50 {
        let _ = refine_store::page_messages(&dbp, &deep, 50, None)?;
        let _ = refine_store::session_exists(&dbp, &deep)?;
    }
    macro_rules! bench {
        ($name:literal, $body:expr) => {{
            let mut samples: Vec<u128> = Vec::with_capacity(reps);
            for _ in 0..reps {
                let t = Instant::now();
                let ok: anyhow::Result<()> = ($body)();
                if let Err(e) = ok {
                    println!("TUNE[{}] {}: ERROR {}", tag, $name, e);
                    break;
                }
                samples.push(t.elapsed().as_micros());
            }
            if samples.len() >= 10 {
                let n_done = samples.len();
                let (p50, p95, mx) = pcts(samples);
                println!(
                    "TUNE[{}] {}: p50={}us p95={}us max={}us n={}",
                    tag, $name, p50, p95, mx, n_done
                );
            }
        }};
    }

    bench!("page_window", || {
        refine_store::page_messages(&dbp, &deep, 50, None)?;
        Ok(())
    });
    bench!("session_exists", || {
        refine_store::session_exists(&dbp, &deep)?;
        Ok(())
    });
    bench!("session_wire", || {
        refine_store::load_session_wire(&dbp, &deep)?;
        Ok(())
    });
    bench!("list_wire", || {
        refine_store::load_sessions_wire(&dbp)?;
        Ok(())
    });
    // cold search paths: memos off (env) — exercises the fts walk + LIKE
    bench!("search_fts", || {
        refine_store::search_parts(&dbp, "the", None, 50, 0)?;
        Ok(())
    });
    bench!("search_like", || {
        refine_store::search_parts(&dbp, "zebra", None, 50, 0)?;
        Ok(())
    });
    bench!("for_each_page", || {
        let (rows, _, _) = refine_store::page_messages(&dbp, &deep, 50, None)?;
        refine_store::for_each_message_json(
            &dbp,
            &deep,
            refine_store::MessageWalk::Window(rows),
            |_| Ok(()),
        )?;
        Ok(())
    });
    Ok(())
}
