//! refine-importer: streaming read-only import of the N most recent sessions.
//! Source DB is never opened read-write (PLAN §9 / STORAGE.md §5).

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

    // Inventory + metadata copy (payload streaming lands with the full importer).
    let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let mut stmt =
        src.prepare(&format!("SELECT id, project_id, slug, directory, title, time_created, time_updated FROM session WHERE id IN ({ph})"))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
        ))
    })?;
    let mut n = 0;
    for row in rows {
        let (id, project_id, slug, directory, title, tc, tu) = row?;
        conn.execute(
            "INSERT OR REPLACE INTO session (id, project_id, slug, directory, title, time_created, time_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![id, project_id, slug, directory, title, tc, tu],
        )?;
        n += 1;
    }
    tracing::info!("imported {n} session rows (metadata pass)");
    // Source connection dropped here — still read-only for its whole life.
    Ok(())
}
