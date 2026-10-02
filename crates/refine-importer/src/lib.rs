//! refine-importer: streaming read-only import of the N most recent sessions.
//! Source DB is never opened read-write (PLAN §9 / STORAGE.md §5).

pub mod payload;

use anyhow::{Context, Result};
use rusqlite::OpenFlags;
use std::path::Path;

/// Import the `limit` most recent sessions from `source` (read-only) into a fresh
/// data dir. Chunked read transactions; never materializes large rows.
pub fn import_last_n(source: &Path, data_dir: &Path, limit: u32) -> Result<()> {
    if !source.exists() {
        anyhow::bail!("source database not found: {}", source.display());
    }
    // READ-ONLY, no write flags at all (PLAN §9): URI mode=ro + query_only.
    let uri = format!(
        "file:{}?mode=ro",
        source
            .to_str()
            .context("source path utf-8")?
            .replace('?', "%3f")
    );
    let src = rusqlite::Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .context("open source read-only")?;
    src.pragma_update(None, "query_only", 1)
        .context("source query_only")?;
    let ver: String = src
        .query_row("SELECT sqlite_version()", [], |r| r.get(0))
        .context("source sqlite version")?;
    tracing::info!("source sqlite {ver}, read-only");

    let ids: Vec<String> = src
        .prepare("SELECT id FROM session ORDER BY time_updated DESC LIMIT ?1")?
        .query_map([limit], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    tracing::info!("selected {} sessions", ids.len());

    std::fs::create_dir_all(data_dir)?;
    let db = refine_store::writer::db_path(data_dir);
    let conn = refine_store::pragma::open_writer(&db)?;
    refine_store::schema::migrate(&conn)?;

    // Full metadata pass (list shape = TUI/oc-remote contract, PLAN F1):
    let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let mut stmt = src.prepare(&format!(
        "SELECT id, project_id, slug, directory, title, version, path, agent, model, cost,
                summary_additions, summary_deletions, summary_files,
                tokens_input, tokens_output, tokens_reasoning,
                tokens_cache_read, tokens_cache_write, time_created, time_updated
         FROM session WHERE id IN ({ph})"
    ))?;
    let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
    let mut n = 0;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        conn.execute(
            "INSERT OR REPLACE INTO session (
                id, project_id, slug, directory, title, version, path, agent, model, cost,
                summary_additions, summary_deletions, summary_files,
                tokens_input, tokens_output, tokens_reasoning,
                tokens_cache_read, tokens_cache_write, time_created, time_updated)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            rusqlite::params![
                id,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, f64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, i64>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, i64>(15)?,
                row.get::<_, i64>(16)?,
                row.get::<_, i64>(17)?,
                row.get::<_, i64>(18)?,
                row.get::<_, i64>(19)?,
            ],
        )?;
        n += 1;
    }
    tracing::info!("imported {n} session rows (metadata pass)");
    // Source connection dropped here — still read-only for its whole life.

    // M3 payload phase: messages/parts/events + search_doc (streamed)
    let stats = payload::import_payloads(source, &db, &ids)?;
    {
        // imported events enter the bounded ring (STORAGE §4)
        let c = refine_store::pragma::open_writer(&db)?;
        let pruned = refine_store::enforce_event_retention(&c)?;
        tracing::info!("event retention enforced at import: {pruned} pruned");
    }
    println!(
        "payloads: {} messages, {} parts ({} blobbed), {} events, {} search_docs in {}ms; peak RSS {} MB",
        stats.messages,
        stats.parts,
        stats.parts_blobbed,
        stats.events,
        stats.search_docs,
        stats.elapsed_ms,
        payload::peak_rss_mb()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "?".into()),
    );
    Ok(())
}
