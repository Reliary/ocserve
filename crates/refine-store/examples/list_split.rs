//! Split the cold list_wire cost: SQL row read vs JSON build. Answers
//! whether M1's remaining time is SQLite or JSON.
use std::time::Instant;
fn main() -> anyhow::Result<()> {
    let db = std::env::args().nth(1).expect("db");
    let p = std::path::PathBuf::from(&db);
    let reps: usize = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    // SQL only: same statement, no serialization
    let t = Instant::now();
    for _ in 0..reps {
        let conn = refine_store::pragma::open_reader(&p)?;
        let mut st = conn.prepare_cached(
            "SELECT id, project_id, directory, path, slug, title, version, agent, model, cost,
                    summary_additions, summary_deletions, summary_files,
                    tokens_input, tokens_output, tokens_reasoning,
                    tokens_cache_read, tokens_cache_write, time_created, time_updated
             FROM session ORDER BY time_updated DESC",
        )?;
        let mut n = 0u64;
        let mut rows = st.query([])?;
        while let Some(row) = rows.next()? {
            let _: String = row.get(0)?;
            let _: f64 = row.get(9)?;
            let _: i64 = row.get(18)?;
            n += 1;
        }
        std::hint::black_box(n);
    }
    let sql_us = t.elapsed().as_micros() as f64 / reps as f64;
    // full M1 bytes path
    let t = Instant::now();
    for _ in 0..reps {
        std::hint::black_box(refine_store::load_sessions_wire_bytes(&p)?);
    }
    let full_us = t.elapsed().as_micros() as f64 / reps as f64;
    println!(
        "list: sql_only={sql_us:.0}us  m1_bytes={full_us:.0}us  json_build={:.0}us",
        full_us - sql_us
    );
    Ok(())
}
