//! M3 payload import: messages/parts/events for the curated session set,
//! streamed in batches (peak RSS gate: never materialize the corpus).
//! Source opened STRICTLY read-only; target via direct connection (WAL allows
//! concurrent readers + the server's single writer; import is an offline op —
//! run before `serve` or while idle).
//!
//! Part bytes > INLINE_PART_MAX spill to the blob store (chunked, zstd) so the
//! 4.5MB-row class cannot bloat SQLite (STORAGE §3).

use anyhow::{Context, Result};
use refine_store::{BlobStore, INLINE_PART_MAX};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;

pub struct PayloadStats {
    pub messages: u64,
    pub parts: u64,
    pub parts_blobbed: u64,
    pub events: u64,
    pub search_docs: u64,
    pub elapsed_ms: u64,
}

/// Read the source DB read-only; write payloads into target db + blobs dir.
pub fn import_payloads(
    source: &Path,
    target_db: &Path,
    session_ids: &[String],
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
        search_docs: 0,
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
            if batch >= 500 {
                dst.execute_batch("COMMIT; BEGIN")?;
                batch = 0;
            }
        }
    }
    dst.execute_batch("COMMIT")?;

    // ---- parts (streamed; large → blobs) ----
    let mut seq_part: HashMap<String, i64> = HashMap::new();
    dst.execute_batch("BEGIN")?;
    {
        let mut stmt = src.prepare(&format!(
            "SELECT id, message_id, session_id, time_created, data FROM part \
             WHERE session_id IN ({ph}) ORDER BY session_id, time_created, id"
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(session_ids))?;
        let mut insert = dst.prepare(
            "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut batch = 0;
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
                (Some(data), None)
            };
            insert.execute(rusqlite::params![
                id, mid, sid, my_seq, ptype, byte_len, inline, sha
            ])?;
            stats.parts += 1;
            batch += 1;
            if batch >= 500 {
                dst.execute_batch("COMMIT; BEGIN")?;
                batch = 0;
            }
        }
    }
    dst.execute_batch("COMMIT")?;

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
            if batch >= 2_000 {
                dst.execute_batch("COMMIT; BEGIN")?;
                batch = 0;
            }
        }
    }
    dst.execute_batch("COMMIT")?;

    // ---- search_doc (session title projection — oc-remote: "not a content
    // search"; external-content FTS rebuilt from this) ----
    dst.execute_batch("BEGIN")?;
    {
        let mut stmt = src.prepare(&format!(
            "SELECT id, title, time_updated FROM session WHERE id IN ({ph})"
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(session_ids))?;
        let mut insert = dst.prepare(
            "INSERT INTO search_doc (id, title, excerpt, updated_at) VALUES (?1, ?2, '', ?3)",
        )?;
        // search_doc.id must be stable INTEGER — use session rowid ordering:
        while let Some(row) = rows.next()? {
            let _sid: String = row.get(0)?;
            let title: String = row.get(1)?;
            let updated: i64 = row.get(2)?;
            let rid = (stats.search_docs + 1) as i64;
            insert.execute(rusqlite::params![rid, title, updated])?;
            stats.search_docs += 1;
        }
        // keep doc ids aligned with... search endpoint LIKEs session.title
        // directly (contract observed); search_doc/FTS = storage gate + future
        // content search. Rebuild index now (external content).
        dst.execute_batch("INSERT INTO search_fts(search_fts) VALUES('rebuild')")?;
    }
    dst.execute_batch("COMMIT")?;

    stats.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(stats)
}

fn session_ph(n: usize) -> String {
    (0..n).map(|_| "?").collect::<Vec<_>>().join(",")
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
