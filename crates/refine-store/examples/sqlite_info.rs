//! One-shot diagnostic: effective pragmas + compile options of OUR
//! bundled rusqlite build (never mutates any db — temp file only).
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let p = dir.path().join("probe.db");
    let conn = refine_store::pragma::create_new(&p)?;
    println!("version = {}", rusqlite::version());
    for k in [
        "analysis_limit",
        "secure_delete",
        "page_size",
        "journal_mode",
        "temp_store",
        "threads",
        "cache_size",
        "mmap_size",
        "wal_autocheckpoint",
        "busy_timeout",
        "cell_size_check",
        "trusted_schema",
        "foreign_keys",
        "synchronous",
        "auto_vacuum",
        "journal_size_limit",
        "wal_autocheckpoint",
    ] {
        let v: String = match conn.query_row(&format!("PRAGMA {k}"), [], |r| {
            r.get::<_, rusqlite::types::Value>(0)
        }) {
            Ok(v) => format!("{v:?}"),
            Err(e) => format!("ERR {e}"),
        };
        println!("{k} = {v}");
    }
    println!("--- compile_options ---");
    let mut stmt = conn.prepare("PRAGMA compile_options")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    for r in rows.flatten() {
        println!("{r}");
    }
    Ok(())
}
