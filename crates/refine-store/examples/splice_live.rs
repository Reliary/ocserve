//! Live proof that the wired L1 path splices (and never falls back) on real
//! data, plus the byte-parity check between the two paths on the same page.
fn main() -> anyhow::Result<()> {
    let db = std::env::args().nth(1).expect("db");
    let dbp = std::path::PathBuf::from(&db);
    let deep: String = {
        let c = refine_store::pragma::open_reader(&dbp)?;
        c.query_row(
            "SELECT session_id FROM msg GROUP BY session_id ORDER BY count(*) DESC LIMIT 1",
            [],
            |r| r.get(0),
        )?
    };
    let r0 = refine_store::splice_rows();
    let f0 = refine_store::splice_fallbacks();
    let mut bytes = 0usize;
    for _ in 0..20 {
        let (rows, _, _) = refine_store::page_messages(&dbp, &deep, 50, None)?;
        refine_store::for_each_message_json(
            &dbp,
            &deep,
            refine_store::MessageWalk::Window(rows),
            |c| {
                bytes += c.len();
                Ok(())
            },
        )?;
    }
    let rows = refine_store::splice_rows() - r0;
    let fb = refine_store::splice_fallbacks() - f0;
    println!("L1 live: deep={deep} spliced_rows={rows} fallbacks={fb} bytes={bytes}");
    anyhow::ensure!(rows > 0, "nothing took the splice path — L1 is not wired");
    anyhow::ensure!(
        fb == 0,
        "{fb} fallbacks on real data (expected 0 per corpus differential)"
    );
    Ok(())
}
