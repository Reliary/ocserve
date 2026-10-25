//! ocserve-importer: streaming read-only import of sessions.
//! Source DB is never opened read-write (PLAN §9 / STORAGE.md §5).
#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod payload;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// Import the `limit` most recent sessions from `source` (read-only) into a fresh
/// data dir. Chunked read transactions; never materializes large rows.
pub fn import_last_n(source: &Path, data_dir: &Path, limit: u32) -> Result<()> {
    let src = open_source_ro(source)?;
    let ids: Vec<String> = src
        .prepare("SELECT id FROM session ORDER BY time_updated DESC LIMIT ?1")?
        .query_map([limit], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    tracing::info!("selected {} sessions", ids.len());
    import_selected(src, source, data_dir, &ids, false, false)
}

/// Import an explicit set of sessions by id (read-only source). `force`
/// removes an already-present target session (with its rows) before importing,
/// which makes re-running an adoption idempotent.
///
/// Unlike `import_last_n`, this is safe against a LIVE target database:
/// the payload phase keeps the incremental FTS trigger in place (no DDL), so
/// a running server writing concurrently can never observe a missing index
/// trigger. The bulk trigger-drop path stays reserved for a fresh data dir.
pub fn import_sessions(
    source: &Path,
    data_dir: &Path,
    ids: &[String],
    force: bool,
    allow_slow_live: bool,
) -> Result<()> {
    if ids.is_empty() {
        bail!("no session ids given");
    }
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if id.trim().is_empty() {
            bail!("empty session id");
        }
        if !seen.insert(id.as_str()) {
            bail!("duplicate session id: {id}");
        }
    }
    let src = open_source_ro(source)?;
    let ph = placeholders(ids.len());
    let found: std::collections::HashSet<String> = src
        .prepare(&format!("SELECT id FROM session WHERE id IN ({ph})"))?
        .query_map(rusqlite::params_from_iter(ids.iter()), |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let missing: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| !found.contains(*id))
        .collect();
    if !missing.is_empty() {
        bail!(
            "session(s) not found in {}: {}",
            source.display(),
            missing.join(", ")
        );
    }
    tracing::info!("selected {} explicit session(s)", ids.len());
    import_selected(src, source, data_dir, ids, force, allow_slow_live)
}

/// Live-target imports index every part through the trigram trigger as it
/// lands. That is safe (no DDL) but its cost grows with the target corpus:
/// measured ~2,100 parts/s on an empty target versus ~170 parts/s on a small
/// populated one, and it degrades from there. A big session adopted into a
/// live database can therefore run for tens of minutes with no progress
/// signal, which looks like a hang. Above this many source parts the
/// caller must opt in explicitly (or stop the server and take the bulk path).
pub const LIVE_IMPORT_PARTS_WARN: i64 = 20_000;

fn source_part_count(src: &Connection, ids: &[String]) -> Result<i64> {
    let ph = placeholders(ids.len());
    let n: i64 = src.query_row(
        &format!("SELECT count(*) FROM part WHERE session_id IN ({ph})"),
        rusqlite::params_from_iter(ids.iter()),
        |r| r.get(0),
    )?;
    Ok(n)
}

/// READ-ONLY, no write flags at all (PLAN §9): URI mode=ro + query_only.
fn open_source_ro(source: &Path) -> Result<Connection> {
    if !source.exists() {
        bail!("source database not found: {}", source.display());
    }
    let uri = format!(
        "file:{}?mode=ro",
        source
            .to_str()
            .context("source path utf-8")?
            .replace('?', "%3f")
    );
    let src = Connection::open_with_flags(
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
    Ok(src)
}

fn placeholders(n: usize) -> String {
    (0..n).map(|_| "?").collect::<Vec<_>>().join(",")
}

/// Metadata pass + payload phase for `ids` (source stays read-only for its
/// whole life; `src` is dropped before the payload reader opens).
fn import_selected(
    src: Connection,
    source: &Path,
    data_dir: &Path,
    ids: &[String],
    force: bool,
    allow_slow_live: bool,
) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let db = ocserve_store::writer::db_path(data_dir);
    let conn = ocserve_store::pragma::open_writer(&db)?;
    ocserve_store::schema::migrate(&conn)?;

    // Incremental vs fresh decides the FTS strategy: a fresh data dir can
    // safely drop/recreate the trigger and bulk-build once; a populated
    // target must never lose the trigger while a server may be writing.
    let existing = count_sessions(&conn)?;
    let bulk_fts = existing == 0;
    tracing::info!(
        "target session count {existing} → {} FTS import",
        if bulk_fts { "bulk" } else { "incremental" }
    );

    // Preflight the slow path before writing anything: a live import of a
    // large session can take tens of minutes because the trigram trigger
    // indexes each part row-by-row, and there is no progress output to
    // distinguish that from a hang. The bulk path (empty target / stopped
    // server) is an order of magnitude faster and is the right answer.
    if !bulk_fts {
        let parts = source_part_count(&src, ids)?;
        if parts > LIVE_IMPORT_PARTS_WARN && !allow_slow_live {
            bail!(
                "refusing a live import of {parts} parts into a populated data dir: the search \
                 trigger indexes every row and this can run for tens of minutes. Stop the \
                 server and re-run (the bulk build is much faster), or pass --allow-slow-live \
                 if you accept the wait."
            );
        }
        tracing::info!("live import: {parts} source part(s) will be indexed incrementally");
    }

    if !bulk_fts {
        let present: Vec<String> = ids
            .iter()
            .filter(|id| session_in(&conn, id))
            .cloned()
            .collect();
        if !present.is_empty() {
            if !force {
                bail!(
                    "session(s) already in {}: {} (re-run with --force to replace)",
                    db.display(),
                    present.join(", ")
                );
            }
            let tx = conn.unchecked_transaction()?;
            for id in &present {
                // event has no FK to session (delete_session removes it by hand)
                tx.execute("DELETE FROM event WHERE session_id = ?1", [id])?;
                // a replaced session must not keep a stale legacy-sync cursor
                tx.execute("DELETE FROM import_sync WHERE session_id = ?1", [id])?;
                tx.execute("DELETE FROM session WHERE id = ?1", [id])?;
            }
            tx.commit()?;
            tracing::info!("replaced {} existing session(s)", present.len());
        }
    }

    // Full metadata pass (list shape = TUI/oc-remote contract, PLAN F1):
    let ph = placeholders(ids.len());
    {
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
    }
    // Source connection dropped here — still read-only for its whole life.
    drop(src);

    // M3 payload phase: messages/parts/events/todos + search (streamed)
    let stats = payload::import_payloads(source, &db, ids, bulk_fts)?;
    {
        // imported events enter the bounded ring (STORAGE §4)
        let c = ocserve_store::pragma::open_writer(&db)?;
        let pruned = ocserve_store::enforce_event_retention(&c)?;
        tracing::info!("event retention enforced at import: {pruned} pruned");
    }
    println!(
        "payloads: {} messages, {} parts ({} blobbed), {} events, {} todos in {}ms; peak RSS {} MB",
        stats.messages,
        stats.parts,
        stats.parts_blobbed,
        stats.events,
        stats.todos,
        stats.elapsed_ms,
        payload::peak_rss_mb()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "?".into()),
    );
    Ok(())
}

fn count_sessions(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT count(*) FROM session", [], |r| r.get(0))
        .context("count target sessions")
}

fn session_in(conn: &Connection, session_id: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM session WHERE id = ?1",
        [session_id],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}
