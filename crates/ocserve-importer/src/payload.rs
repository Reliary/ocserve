//! M3 payload import: messages/parts/events for the curated session set,
//! streamed in batches (peak RSS gate: never materialize the corpus).
//! Source opened STRICTLY read-only; target via direct connection (WAL allows
//! concurrent readers + the server's single writer; import is an offline op —
//! run before `serve` or while idle).
//!
//! Part bytes > INLINE_PART_MAX spill to the blob store (chunked, zstd) so the
//! 4.5MB-row class cannot bloat SQLite (STORAGE §3).

use anyhow::{Context, Result};
use ocserve_store::{BlobStore, INLINE_PART_MAX};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;

pub struct PayloadStats {
    pub messages: u64,
    pub parts: u64,
    pub parts_blobbed: u64,
    pub events: u64,
    pub todos: u64,
    pub elapsed_ms: u64,
}

/// Read the source DB read-only; write payloads into target db + blobs dir.
///
/// `bulk_fts` selects the index strategy. `true` = the target was empty (or
/// this is an offline import): drop `part_search_ai`, stream the parts, then
/// rebuild the trigram index once. `false` = the target is live/populated:
/// KEEP the trigger and let each insert index itself. The incremental path is
/// slower but issues no DDL, so a concurrently running server never sees a
/// missing trigger (the crash-window class in `backfill_part_search`).
pub fn import_payloads(
    source: &Path,
    target_db: &Path,
    session_ids: &[String],
    bulk_fts: bool,
) -> Result<PayloadStats> {
    let t0 = std::time::Instant::now();
    let src = Connection::open_with_flags(
        source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .context("open source read-only")?;
    src.busy_timeout(std::time::Duration::from_millis(10_000))?;
    let dst = Connection::open(target_db).context("open target")?;
    dst.busy_timeout(std::time::Duration::from_millis(10_000))?;
    let blobs = BlobStore::new(
        target_db
            .parent()
            .context("target db parent")?
            .join("blobs"),
    )?;
    // Commit granularity: the bulk path owns the data dir and can commit big
    // batches (fewer WAL flushes feed the FTS rebuild). The incremental path
    // runs against a LIVE db, so each write transaction must stay short — a
    // long-held write lock makes the running server's next write hit its own
    // busy_timeout and fail a prompt. Parts are the slow rows (the trigram
    // trigger indexes each one), so that batch is the tight one.
    let (msg_batch, part_batch, event_batch) = if bulk_fts {
        (500usize, 5_000usize, 2_000usize)
    } else {
        (200usize, 40usize, 250usize)
    };

    // session time map (event timestamps approximate to session age — source
    // events carry no timestamps; documented in importer)
    let ph = session_ph(session_ids.len());
    let mut times: HashMap<String, i64> = HashMap::new();
    {
        let mut stmt = src.prepare(&format!(
            "SELECT id, time_updated FROM session WHERE id IN ({ph})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(session_ids), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows.flatten() {
            times.insert(row.0, row.1);
        }
    }

    // ---- messages (streamed, per-session seq) ----
    let mut seq_msg: HashMap<String, i64> = HashMap::new();
    let mut msg_seq_map: HashMap<String, i64> = HashMap::new(); // source msg id → our seq
    let mut stats = PayloadStats {
        messages: 0,
        parts: 0,
        parts_blobbed: 0,
        events: 0,
        todos: 0,
        elapsed_ms: 0,
    };
    dst.execute_batch("BEGIN")?;
    {
        let mut stmt = src.prepare(&format!(
            "SELECT id, session_id, time_created, data FROM message \
             WHERE session_id IN ({ph}) ORDER BY session_id, time_created, id"
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(session_ids))?;
        let mut insert = dst.prepare(
            "INSERT INTO msg (id, session_id, role, seq, time_created, info) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut batch = 0;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let sid: String = row.get(1)?;
            let created: i64 = row.get(2)?;
            let data: String = row.get(3)?;
            let role: String = serde_json::from_str::<serde_json::Value>(&data)
                .ok()
                .and_then(|v| v["role"].as_str().map(String::from))
                .unwrap_or_else(|| "user".into());
            let seq = seq_msg.entry(sid.clone()).or_insert(0);
            *seq += 1;
            let my_seq = *seq;
            insert.execute(rusqlite::params![id, sid, role, my_seq, created, data])?;
            msg_seq_map.insert(id, my_seq);
            stats.messages += 1;
            batch += 1;
            if batch >= msg_batch {
                dst.execute_batch("COMMIT; BEGIN")?;
                batch = 0;
            }
        }
    }
    dst.execute_batch("COMMIT")?;

    // ---- parts (streamed; large → blobs) ----
    // FTS bulk-load fast path (2026-10-11 perf dive): the per-insert
    // `part_search_ai` trigger makes a full import O(N·M) — the trigram
    // index's segment auto-merge cascade collapses the rate from ~6,800
    // parts/s (bulk) to ~20-27 parts/s as the corpus grows (measured on the
    // 170k-part/162MB fixture corpus: 25s rebuild vs >2h trigger-driven; the
    // real 1.6GB live corpus measured 310s for its FTS rebuild in W1). When
    // `bulk_fts`, the trigger is DROPPED for the streaming phase, one
    // `'rebuild'` bulk-builds the index, then the trigger is recreated — ALL
    // inside ONE transaction, so an interrupted import rolls back to the
    // schema migrate() left (no window where future writes silently skip the
    // index). A live/populated target NEVER takes this path (see
    // `import_payloads`): it keeps the trigger and pays the incremental cost.
    let mut seq_part: HashMap<String, i64> = HashMap::new();
    let t_parts = std::time::Instant::now();
    // Batched commits (2026-10-11): ONE 178k-row transaction left the pager
    // holding every dirty page through the FTS build — measured 3.3 GB/s of
    // page reads and >30 min for the rebuild vs 25 s when the part rows are
    // committed first (WAL checkpoints between batches, clean pages feed the
    // bulk build). The trigger drop rides the first batch's transaction.
    dst.execute_batch("BEGIN")?;
    if bulk_fts {
        dst.execute_batch("DROP TRIGGER IF EXISTS part_search_ai")?;
    }
    {
        let mut stmt = src.prepare(&format!(
            "SELECT id, message_id, session_id, time_created, data FROM part \
             WHERE session_id IN ({ph}) ORDER BY session_id, time_created, id"
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(session_ids))?;
        let mut batch = 0u64;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let mid: String = row.get(1)?;
            let sid: String = row.get(2)?;
            let data: String = row.get(4)?;
            let ptype: String = serde_json::from_str::<serde_json::Value>(&data)
                .ok()
                .and_then(|v| v["type"].as_str().map(String::from))
                .unwrap_or_else(|| "text".into());
            let seq = seq_part.entry(mid.clone()).or_insert(0);
            *seq += 1;
            let my_seq = *seq;
            let byte_len = data.len() as i64;
            let (inline, sha): (Option<String>, Option<String>) = if data.len() > INLINE_PART_MAX {
                let (sha, _, _) = blobs.put(data.as_bytes())?;
                stats.parts_blobbed += 1;
                (None, Some(sha))
            } else {
                (Some(data.clone()), None)
            };
            // part_row_ops = msg_part row + part_search projection (W1);
            // check-guards rule 4 keeps msg_part INSERTs inside ocserve-store
            let ops = ocserve_store::part_row_ops(&ocserve_store::PartRow {
                id: &id,
                message_id: &mid,
                session_id: &sid,
                seq: my_seq,
                ptype: &ptype,
                byte_len,
                inline,
                blob_sha: sha,
                text: &data,
            });
            ocserve_store::apply_ops(&dst, &ops)?;
            stats.parts += 1;
            batch += 1;
            if batch >= part_batch as u64 {
                dst.execute_batch("COMMIT; BEGIN")?;
                batch = 0;
            }
            if stats.parts.is_multiple_of(40_000) {
                let el = t_parts.elapsed().as_secs_f64();
                tracing::info!(
                    "parts progress: {} in {el:.1}s ({:.0}/s)",
                    stats.parts,
                    stats.parts as f64 / el
                );
            }
        }
    }
    dst.execute_batch("COMMIT")?;
    let t_ins = t_parts.elapsed().as_secs_f64();
    tracing::info!(
        "parts insert phase: {} rows in {t_ins:.1}s ({:.0}/s)",
        stats.parts,
        stats.parts as f64 / t_ins.max(0.001)
    );
    let dst = if bulk_fts {
        tracing::info!("starting FTS bulk build");
        // Bulk FTS build on a FRESH connection after a WAL checkpoint
        // (2026-10-11, measured): rebuilding on the import connection while
        // its 1.1 GB WAL is pending takes >16 min on the .227 fixture corpus
        // (page reads of the uncheckpointed WAL dominate); the identical
        // rebuild on a fresh connection after `wal_checkpoint(TRUNCATE)` =
        // 113 s. So: checkpoint, drop the import connection, reopen with the
        // writer profile, rebuild, recreate the trigger. Crash windows
        // (killed mid-rebuild) are closed by the boot self-heal in
        // `backfill_part_search` (docsize < part_search count + missing
        // trigger both repaired there).
        drop(dst);
        {
            let ck = Connection::open(target_db).context("reopen for checkpoint")?;
            ck.busy_timeout(std::time::Duration::from_millis(10_000))?;
            ck.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .context("wal_checkpoint before fts rebuild")?;
        }
        let dst = ocserve_store::pragma::open_writer(target_db).context("reopen writer for fts")?;
        let t_rb = std::time::Instant::now();
        dst.execute_batch("INSERT INTO part_search_fts(part_search_fts) VALUES('rebuild')")
            .context("fts bulk rebuild")?;
        tracing::info!("fts bulk build: {:.1}s", t_rb.elapsed().as_secs_f64());
        dst.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS part_search_ai AFTER INSERT ON part_search BEGIN
                 INSERT INTO part_search_fts(rowid, text) VALUES (new.rowid, new.text);
             END",
        )?;
        dst
    } else {
        // Incremental path: the trigger indexed every row as it landed. Only
        // assert the guard invariant (never DDL on a live DB).
        let trigger: i64 = dst.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name='part_search_ai'",
            [],
            |r| r.get(0),
        )?;
        if trigger != 1 {
            anyhow::bail!("incremental import: part_search_ai trigger missing from target");
        }
        dst
    };
    // Stamp the parity marker (boot-cost gate, perf dive 2026-10-11): the
    // server's boot check then skips two O(N) counts on every restart; the
    // stamp is validated against fts docsize + trigger presence, so a crash
    // between rebuild and stamp keeps the old (or missing) value and the
    // full check still runs.
    {
        let docs: i64 = dst
            .query_row("SELECT count(*) FROM part_search_fts_docsize", [], |r| {
                r.get(0)
            })
            .context("docsize after fts rebuild")?;
        dst.execute(
            "INSERT OR REPLACE INTO import_manifest (key, value) VALUES ('parts_indexed', ?1)",
            [docs],
        )?;
    }

    // ---- todos (session task lists: the TUI/web panel reads GET /todo) ----
    // Upstream keys todos by (session_id, position) and has NO id column;
    // ocserve's todo table has a stable `id` PK, so the id is derived from
    // the session id + position (the same shape the todowrite runner mints).
    // Older sources predate the table: skip it rather than failing the import.
    let has_todos = source_has_table(&src, "todo")?;
    if has_todos {
        dst.execute_batch("BEGIN")?;
        {
            let mut stmt = src.prepare(&format!(
                "SELECT session_id, content, status, priority, position, time_created, time_updated \
                 FROM todo WHERE session_id IN ({ph}) ORDER BY session_id, position"
            ))?;
            let mut rows = stmt.query(rusqlite::params_from_iter(session_ids))?;
            let mut insert = dst.prepare(
                "INSERT OR REPLACE INTO todo \
                 (id, session_id, content, status, priority, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            while let Some(row) = rows.next()? {
                let sid: String = row.get(0)?;
                let position: i64 = row.get(4)?;
                insert.execute(rusqlite::params![
                    format!("{sid}_t{position}"),
                    sid,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ])?;
                stats.todos += 1;
            }
        }
        dst.execute_batch("COMMIT")?;
    } else {
        tracing::info!("source has no todo table — skipping session task lists");
    }

    // ---- events (bounded ring input; time ≈ session updated) ----
    dst.execute_batch("BEGIN")?;
    {
        let mut stmt = src.prepare(&format!(
            "SELECT aggregate_id, type, data FROM event \
             WHERE aggregate_id IN ({ph}) ORDER BY aggregate_id, seq"
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(session_ids))?;
        let mut insert = dst.prepare(
            "INSERT INTO event (session_id, project_id, type, payload, time_created) \
             VALUES (?1, 'global', ?2, ?3, ?4)",
        )?;
        let mut batch = 0;
        while let Some(row) = rows.next()? {
            let sid: String = row.get(0)?;
            let etype: String = row.get(1)?;
            let data: String = row.get(2)?;
            let ts = times.get(&sid).copied().unwrap_or(0);
            insert.execute(rusqlite::params![sid, etype, data, ts])?;
            stats.events += 1;
            batch += 1;
            if batch >= event_batch {
                dst.execute_batch("COMMIT; BEGIN")?;
                batch = 0;
            }
        }
    }
    dst.execute_batch("COMMIT")?;

    stats.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(stats)
}

fn session_ph(n: usize) -> String {
    (0..n).map(|_| "?").collect::<Vec<_>>().join(",")
}

/// Source-table existence probe (older schemas lack `todo`).
fn source_has_table(src: &Connection, name: &str) -> Result<bool> {
    let n: i64 = src.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name = ?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Peak RSS (VmHWM) in MB — import memory KPI evidence (MEMORY §gate).
/// /proc reports VmHWM in kB — convert (first run printed kB as MB).
pub fn peak_rss_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmHWM:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())
        .map(|kb| kb / 1024)
}
