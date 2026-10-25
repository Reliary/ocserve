//! ocserve-importer: streaming read-only import of sessions.
//! Source DB is never opened read-write (PLAN §9 / STORAGE.md §5).
#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod payload;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// How to treat a live (running) server while indexing.
///
/// The trigram trigger indexes each part as it lands: safe for a running
/// server, but its cost grows with the target corpus (measured ~2,100 parts/s
/// into an empty target, ~170 parts/s into a small populated one, and a
/// 21k-part adoption exceeded 15 minutes). The bulk path drops the trigger,
/// streams the parts, rebuilds the index once and recreates the trigger — an
/// order of magnitude faster, but it must never run while another process can
/// write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndexMode {
    /// Choose automatically: bulk for an empty target, live otherwise.
    Auto,
    /// Force the live (trigger-driven) path even for a large session.
    LiveSlow,
    /// Force the bulk path. The caller asserts that no server is writing.
    Bulk,
}

/// Import the `limit` most recent sessions from `source` (read-only) into a fresh
/// data dir. Chunked read transactions; never materializes large rows.
pub fn import_last_n(source: &Path, data_dir: &Path, limit: u32) -> Result<()> {
    let src = open_source_ro(source)?;
    let ids: Vec<String> = src
        .prepare("SELECT id FROM session ORDER BY time_updated DESC LIMIT ?1")?
        .query_map([limit], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    tracing::info!("selected {} sessions", ids.len());
    import_selected(src, source, data_dir, &ids, false, IndexMode::Auto)
}

/// Import an explicit set of sessions by id (read-only source). `force`
/// removes an already-present target session (with its rows) before importing,
/// which makes re-running an adoption idempotent.
///
/// `mode` controls the search-index strategy. `Auto` keeps the incremental
/// trigger while the target already has sessions (safe against a running
/// server) and uses the bulk rebuild for an empty target. `Bulk` forces the
/// rebuild and must only be used while no server is running; `LiveSlow`
/// accepts the per-row trigger cost while a server keeps running.
pub fn import_sessions(
    source: &Path,
    data_dir: &Path,
    ids: &[String],
    force: bool,
    mode: IndexMode,
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
    import_selected(src, source, data_dir, ids, force, mode)
}

/// Live-target imports index every part through the trigram trigger as it
/// lands. That is safe (no DDL) but its cost grows with the target corpus:
/// measured 5,722 parts in ~40 s (~140/s) on a small populated target, and a
/// 21k-message / 80k-part adoption exceeded 15 minutes with no progress output,
/// which looks like a hang. Above this many source parts the caller must opt
/// in explicitly (`--allow-slow-live`, or `--bulk` with the server stopped).
///
/// The bulk path is not automatically faster: it replaces per-row indexing
/// with one full trigram rebuild, which for the 64.7 MB / 5,722-part session
/// measured ~60 s on its own. It wins when the session is large relative to
/// the trigger's superlinear cost, which is what this threshold approximates.
pub const LIVE_IMPORT_PARTS_WARN: i64 = 10_000;

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
    mode: IndexMode,
) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let db = ocserve_store::writer::db_path(data_dir);
    let conn = ocserve_store::pragma::open_writer(&db)?;
    ocserve_store::schema::migrate(&conn)?;

    // Incremental vs fresh decides the FTS strategy: a fresh data dir can
    // safely drop/recreate the trigger and bulk-build once; a populated
    // target must never lose the trigger while a server may be writing.
    let existing = count_sessions(&conn)?;
    let bulk_fts = match mode {
        IndexMode::Bulk => true,
        IndexMode::LiveSlow => false,
        IndexMode::Auto => existing == 0,
    };
    tracing::info!(
        "target session count {existing} → {} FTS import ({mode:?})",
        if bulk_fts { "bulk" } else { "incremental" }
    );
    if bulk_fts && existing > 0 {
        // The caller asserted the server is stopped. Say it out loud: the
        // trigger is dropped for the duration of the import, and a concurrent
        // writer would insert rows that never reach the index.
        tracing::warn!(
            "--bulk on a populated data dir: the search trigger is dropped until the rebuild \
             finishes. A server must not be writing while this runs."
        );
    }

    // Preflight the slow path before writing anything: a live import of a
    // large session can take tens of minutes because the trigram trigger
    // indexes each part row-by-row, and there is no progress output to
    // distinguish that from a hang. Stopping the server alone changes nothing
    // (the mode is chosen from the target's contents); `--bulk` is what makes
    // the stopped case fast. It trades the per-row cost for one full index
    // rebuild, so it wins on large sessions and can lose on small ones.
    if !bulk_fts {
        let parts = source_part_count(&src, ids)?;
        if parts > LIVE_IMPORT_PARTS_WARN && mode != IndexMode::LiveSlow {
            bail!(
                "refusing a live import of {parts} parts into a populated data dir: the search \
                 trigger indexes every row and this can run for tens of minutes. Stop the \
                 server and re-run with --bulk (fast for large sessions; it rebuilds the whole \
                 index once), or pass --allow-slow-live if the server must keep running."
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
                    // `session.path` is nullable in the upstream schema (a
                    // session that never had one stored). Reading it as
                    // String aborted the whole import with
                    // `Invalid column type Null` — found 2026-10-25 on a real
                    // source row. The target column is NOT NULL DEFAULT ''.
                    row.get::<_, Option<String>>(6)?.unwrap_or_default(),
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
