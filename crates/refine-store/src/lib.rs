//! refine-store: SQLite (pragmas/schema/writer) + chunked blob store.
//!
//! Invariants (see STORAGE.md / MEMORY.md / AGENTS.md):
//! - PRAGMAs only via `pragma::{create_new,open_writer,open_reader}`
//! - no connection held across `.await` (enforced by the scoped `Writer` API)
//! - payloads via `blob::BlobStore`, never as SQLite row payloads > metadata

pub mod blob;
pub mod pragma;
pub mod schema;
pub mod writer;

pub use blob::BlobStore;
pub use writer::{WriteOp, Writer};

/// Upstream wire shape of a session list entry (PLAN F1; keys verified against
/// recorded manifest session_list keys).
pub fn load_sessions_wire(db: &std::path::Path) -> anyhow::Result<Vec<serde_json::Value>> {
    let conn = pragma::open_reader(db)?;
    let mut stmt = conn.prepare(
        "SELECT id, project_id, directory, path, slug, title, version, agent, model, cost,
                summary_additions, summary_deletions, summary_files,
                tokens_input, tokens_output, tokens_reasoning,
                tokens_cache_read, tokens_cache_write, time_created, time_updated
         FROM session ORDER BY time_updated DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        let model_txt: Option<String> = r.get(8)?;
        Ok(serde_json::json!({
            "id": r.get::<_, String>(0)?,
            "projectID": r.get::<_, String>(1)?,
            "directory": r.get::<_, String>(2)?,
            "path": r.get::<_, String>(3)?,
            "slug": r.get::<_, String>(4)?,
            "title": r.get::<_, String>(5)?,
            "version": r.get::<_, String>(6)?,
            "agent": r.get::<_, Option<String>>(7)?,
            "model": model_txt.as_deref().and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
                .unwrap_or_else(|| serde_json::json!({"id":"","providerID":"","variant":"default"})),
            "cost": r.get::<_, f64>(9)?,
            "summary": {"additions": r.get::<_, i64>(10)?, "deletions": r.get::<_, i64>(11)?, "files": r.get::<_, i64>(12)?},
            "tokens": {
                "input": r.get::<_, i64>(13)?,
                "output": r.get::<_, i64>(14)?,
                "reasoning": r.get::<_, i64>(15)?,
                "cache": {"read": r.get::<_, i64>(16)?, "write": r.get::<_, i64>(17)?},
            },
            "time": {"created": r.get::<_, i64>(18)?, "updated": r.get::<_, i64>(19)?},
        }))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Insert one message + its parts in a single writer batch (wire shape:
/// msg.info holds the full upstream info JSON; parts inline ≤8KB else blob).
/// Inline threshold (MEMORY §6 / STORAGE §4): parts ≤8 KiB live in-row,
/// larger payloads go to the chunked blob store (sha referenced, never inline).
pub const INLINE_PART_MAX: usize = 8 * 1024;

pub fn insert_message(
    writer: &Writer,
    blobs: Option<&BlobStore>,
    session_id: &str,
    info: &serde_json::Value,
    parts: &[serde_json::Value],
) -> anyhow::Result<()> {
    let id = info["id"].as_str().unwrap_or_default().to_string();
    let role = info["role"].as_str().unwrap_or("user").to_string();
    let time_created = info["time"]["created"].as_i64().unwrap_or(0);
    let mut ops = vec![
        WriteOp::Sql {
            sql: "INSERT OR REPLACE INTO msg (id, session_id, role, seq, time_created, info) \
                  VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(seq),0)+1 FROM msg WHERE session_id=?2), ?4, ?5)"
                .into(),
            params: vec![
                id.clone().into(),
                session_id.into(),
                role.into(),
                time_created.into(),
                info.to_string().into(),
            ],
        },
        WriteOp::Sql {
            sql: "UPDATE session SET time_updated = MAX(time_updated, ?2) WHERE id = ?1".into(),
            params: vec![session_id.into(), time_created.into()],
        },
    ];
    for p in parts {
        let pid = p["id"].as_str().unwrap_or_default().to_string();
        let ptype = p["type"].as_str().unwrap_or("text").to_string();
        let text = p.to_string();
        let byte_len = text.len() as i64;
        let (inline, blob_sha) = match blobs.filter(|_| text.len() > INLINE_PART_MAX) {
            Some(store) => {
                let (sha, _, _) = store.put(text.as_bytes())?;
                (None, Some(sha))
            }
            None => (Some(text), None),
        };
        ops.push(WriteOp::Sql {
            sql: "INSERT OR REPLACE INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) \
                  VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(seq),0)+1 FROM msg_part WHERE message_id=?2), ?4, ?5, ?6, ?7)"
                .into(),
            params: vec![
                pid.clone().into(),
                id.clone().into(),
                session_id.into(),
                ptype.into(),
                byte_len.into(),
                inline.into(),
                blob_sha.into(),
            ],
        });
    }
    writer.write(ops).map(|_| ())
}

/// Load history for prompt assembly: (info, parts) ordered by msg.seq.
/// `limit` = return the LAST n messages ascending (upstream `?limit=` live
/// contract §1084); None = full history.
/// Cursor format — byte-compatible with upstream MessageV2.cursor
/// (message-v2.ts:71-84): base64url(JSON {"id","time"}), no padding.
pub fn encode_cursor(id: &str, time_created: i64) -> String {
    use base64::Engine as _;
    let json = format!("{{\"id\":\"{id}\",\"time\":{time_created}}}");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

/// Decode an upstream-shaped cursor. Errors on malformed base64, bad JSON,
/// non-`msg_` ids, or negative time — mirrors upstream's Schema checks
/// (id: MessageID brand, time ≥ 0) closely enough that garbage 400s.
pub fn decode_cursor(s: &str) -> anyhow::Result<(String, i64)> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|e| anyhow::anyhow!("cursor base64: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| anyhow::anyhow!("cursor json: {e}"))?;
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
    let time = v.get("time").and_then(|x| x.as_i64());
    match (id.starts_with("msg_"), time) {
        (true, Some(t)) if t >= 0 => Ok((id.to_string(), t)),
        _ => anyhow::bail!("cursor fields"),
    }
}

/// One paged window (upstream MessageV2.page semantics): newest `limit`
/// messages older than the cursor, tuple-ordered `(time_created DESC,
/// id DESC) LIMIT limit+1` → extra row signals `more` → reversed to ASC.
/// The cursor is derived from the OLDEST kept row. Returns the window's
/// ordered message (id, info) rows, `more`, and the next cursor.
/// (window rows ASC, more, next-cursor)
pub type PageWindow = (Vec<(String, String)>, bool, Option<String>);

pub fn page_messages(
    db: &std::path::Path,
    session_id: &str,
    limit: u64,
    before: Option<(&str, i64)>,
) -> anyhow::Result<PageWindow> {
    let conn = pragma::open_reader(db)?;
    let n = limit.saturating_add(1);
    let rows: Vec<(String, String, i64)> = match before {
        Some((bid, btime)) => {
            let mut stmt = conn.prepare(
                "SELECT id, info, time_created FROM msg WHERE session_id = ?1 \
                 AND (time_created < ?2 OR (time_created = ?2 AND id < ?3)) \
                 ORDER BY time_created DESC, id DESC LIMIT ?4",
            )?;
            stmt.query_map((session_id, btime, bid, n as i64), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .filter_map(|r| r.ok())
            .collect()
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, info, time_created FROM msg WHERE session_id = ?1 \
                 ORDER BY time_created DESC, id DESC LIMIT ?2",
            )?;
            stmt.query_map((session_id, n as i64), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .filter_map(|r| r.ok())
            .collect()
        }
    };
    let more = rows.len() as u64 > limit;
    let mut kept: Vec<(String, String, i64)> = rows;
    if more {
        kept.pop(); // the +1 probe row
    }
    let next = if more {
        kept.last().map(|(id, _, t)| encode_cursor(id, *t))
    } else {
        None
    };
    kept.reverse(); // ASC
    Ok((
        kept.into_iter().map(|(i, info, _)| (i, info)).collect(),
        more,
        next,
    ))
}

/// Walk a session's messages one at a time, yielding each message's FULL
/// response JSON object (`{"info":…,"parts":[…]}`) to `visit`. Bounded by
/// design (AGENTS §2.3): peak = one message — the old Vec<Value> path
/// OOM-killed the cgroup on a 16k-message session (93MB response ≈ 400MB+
/// as serde Values). Column merge applied per row (see merge_columns).
/// What to walk: full/tail history (seq order) or an explicit pre-ordered
/// cursor window from `page_messages` (tuple order).
pub enum MessageWalk {
    /// last `limit` messages in seq order (`None` = all)
    Seq { limit: Option<usize> },
    /// pre-computed (id, info) window in response order (already ASC)
    Window(Vec<(String, String)>),
}

pub fn for_each_message_json(
    db: &std::path::Path,
    session_id: &str,
    walk: MessageWalk,
    mut visit: impl FnMut(String) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let conn = pragma::open_reader(db)?;
    let blobs = crate::blob::BlobStore::new(
        db.parent()
            .ok_or_else(|| anyhow::anyhow!("db parent"))?
            .join("blobs"),
    )?;
    let msgs: Vec<(String, String)> = match walk {
        MessageWalk::Window(w) => w,
        MessageWalk::Seq { limit } => {
            // (time_created, id) — same tuple as cursor pages; merged
            // legacy/refine timelines stay chronological (seq = write-order
            // bookkeeping only; sync appends older-by-time rows later)
            let mut stmt = match limit {
                Some(_) => conn.prepare(
                    "SELECT id, info FROM (SELECT id, info, time_created FROM msg WHERE session_id = ?1 \
                     ORDER BY time_created DESC, id DESC LIMIT ?2) ORDER BY time_created, id",
                )?,
                None => conn.prepare(
                    "SELECT id, info FROM msg WHERE session_id = ?1 ORDER BY time_created, id",
                )?,
            };
            match limit {
                Some(n) => stmt
                    .query_map((session_id, n as i64), |r| Ok((r.get(0)?, r.get(1)?)))?
                    .filter_map(|r| r.ok())
                    .collect(),
                None => stmt
                    .query_map([session_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .filter_map(|r| r.ok())
                    .collect(),
            }
        }
    };
    let mut pstmt = conn.prepare(
        "SELECT id, inline, blob_sha, byte_len FROM msg_part WHERE message_id = ?1 ORDER BY seq",
    )?;
    for (mid, info_txt) in msgs {
        // string-only assembly: parse→merge→serialize per row, never a
        // json!-wrapper Value tree (the wrapper roughly doubled transient
        // churn during the 101MB stream — measured RSS 599/600MB)
        let info: serde_json::Value = serde_json::from_str(&info_txt)?;
        let info = merge_columns(info, &mid, session_id, None);
        let mut chunk = String::with_capacity(info_txt.len() + 1024);
        chunk.push_str("{\"info\":");
        chunk.push_str(&info.to_string());
        chunk.push_str(",\"parts\":[");
        let mut rows = pstmt.query([&mid])?;
        let mut first_part = true;
        while let Some(row) = rows.next()? {
            let part_id: String = row.get(0)?;
            let inline: Option<String> = row.get(1)?;
            let sha: Option<String> = row.get(2)?;
            let byte_len: i64 = row.get(3)?;
            let txt = match inline {
                Some(t) => t,
                None => match (&sha, byte_len) {
                    (Some(sha), len) => {
                        String::from_utf8_lossy(&blobs.get(sha, len as u64)?).into_owned()
                    }
                    (None, _) => continue,
                },
            };
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                let merged = merge_columns(v, &part_id, session_id, Some(&mid));
                if !first_part {
                    chunk.push(',');
                }
                first_part = false;
                chunk.push_str(&merged.to_string());
            }
        }
        chunk.push_str("]}");
        visit(chunk)?;
    }
    Ok(())
}

/// Response-time column merge — v1 parity (source store keeps `data` blobs
/// WITHOUT id/sessionID(/messageID); those live in columns and are merged
/// into every response. Our own writes already carry them; the merge is
/// authoritative-from-column either way — same as upstream serving).
fn merge_columns(
    mut v: serde_json::Value,
    id: &str,
    session_id: &str,
    message_id: Option<&str>,
) -> serde_json::Value {
    v["id"] = serde_json::Value::String(id.to_string());
    v["sessionID"] = serde_json::Value::String(session_id.to_string());
    if let Some(mid) = message_id {
        v["messageID"] = serde_json::Value::String(mid.to_string());
    }
    v
}

pub fn load_messages(
    db: &std::path::Path,
    session_id: &str,
    limit: Option<usize>,
) -> anyhow::Result<Vec<(serde_json::Value, Vec<serde_json::Value>)>> {
    let conn = pragma::open_reader(db)?;
    let mut stmt = match limit {
        Some(_) => conn.prepare(
            "SELECT id, info FROM (SELECT id, info, seq FROM msg WHERE session_id = ?1 \
             ORDER BY seq DESC LIMIT ?2) ORDER BY seq",
        )?,
        None => conn.prepare("SELECT id, info FROM msg WHERE session_id = ?1 ORDER BY seq")?,
    };
    let msgs: Vec<(String, String)> = match limit {
        Some(n) => stmt
            .query_map((session_id, n as i64), |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect(),
        None => stmt
            .query_map([session_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect(),
    };
    let mut out = Vec::new();
    // Blob-resolving reader: parts above INLINE_PART_MAX live in the blob
    // store (import + prompt paths both write them); never silently drop.
    let blobs = crate::blob::BlobStore::new(
        db.parent()
            .ok_or_else(|| anyhow::anyhow!("db parent"))?
            .join("blobs"),
    )?;
    for (mid, info_txt) in msgs {
        let info: serde_json::Value = serde_json::from_str(&info_txt)?;
        let info = merge_columns(info, &mid, session_id, None);
        let mut pstmt = conn.prepare(
            "SELECT id, inline, blob_sha, byte_len FROM msg_part WHERE message_id = ?1 ORDER BY seq",
        )?;
        let mut parts: Vec<serde_json::Value> = Vec::new();
        let mut rows = pstmt.query([&mid])?;
        while let Some(row) = rows.next()? {
            let part_id: String = row.get(0)?;
            let inline: Option<String> = row.get(1)?;
            let sha: Option<String> = row.get(2)?;
            let byte_len: i64 = row.get(3)?;
            let txt = match inline {
                Some(t) => t,
                None => match (&sha, byte_len) {
                    (Some(sha), len) => {
                        String::from_utf8_lossy(&blobs.get(sha, len as u64)?).into_owned()
                    }
                    (None, _) => continue, // corrupt row: no payload at all
                },
            };
            if let Ok(v) = serde_json::from_str(&txt) {
                parts.push(merge_columns(v, &part_id, session_id, Some(&mid)));
            }
        }
        out.push((info, parts));
    }
    Ok(out)
}

/// Per-session event sequence (sync twin numbering; capture showed 1..N per session).
pub fn next_event_seq(db: &std::path::Path, session_id: &str) -> anyhow::Result<i64> {
    let conn = pragma::open_reader(db)?;
    let seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq),0)+1 FROM event WHERE session_id = ?1",
        [session_id],
        |r| r.get(0),
    )?;
    Ok(seq)
}

/// Ring bound (STORAGE §4): per-session event cap + age window — declared
/// at the call site (AGENTS §2.3: bounded by construction).
pub const EVENT_MAX_PER_SESSION: i64 = 200_000;
pub const EVENT_MAX_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Enforce the ring bound: keep the newest `max_per_session` rows per
/// session and drop rows older than `max_age_ms` (explicit `now_ms` for
/// deterministic tests). Returns rows deleted.
pub fn prune_events(
    conn: &rusqlite::Connection,
    now_ms: i64,
    max_per_session: i64,
    max_age_ms: i64,
) -> anyhow::Result<usize> {
    let cutoff = now_ms - max_age_ms;
    conn.execute(
        "DELETE FROM event WHERE time_created < ?1 AND time_created <> 0",
        [cutoff],
    )?;
    let aged = conn.changes() as usize;
    let capped = conn.execute(
        "DELETE FROM event WHERE rowid IN (
            SELECT rowid FROM (
                SELECT rowid, ROW_NUMBER() OVER (
                    PARTITION BY session_id ORDER BY seq DESC
                ) AS rn FROM event
            ) WHERE rn > ?1
        )",
        [max_per_session],
    )?;
    Ok(aged + capped)
}

/// Retention with the production bounds (boot + import + throttled append).
pub fn enforce_event_retention(conn: &rusqlite::Connection) -> anyhow::Result<usize> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    prune_events(conn, now, EVENT_MAX_PER_SESSION, EVENT_MAX_AGE_MS)
}

/// Persist a durable event (ring bound enforced at boot/import/append).
pub fn append_event(
    writer: &Writer,
    session_id: Option<&str>,
    event_type: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    writer.write(vec![WriteOp::Sql {
        sql: "INSERT INTO event (session_id, project_id, type, payload, time_created) \
              VALUES (?1, 'global', ?2, ?3, ?4)"
            .into(),
        params: vec![
            session_id.map(|s| s.to_string()).into(),
            event_type.into(),
            payload.to_string().into(),
            now.into(),
        ],
    }])?;
    // Throttled ring enforcement (STORAGE §4): every 1024th durable write
    // enqueues the retention deletes — an in-band bound, not a cron hope.
    static EVENT_PRUNE_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    use std::sync::atomic::Ordering;
    if EVENT_PRUNE_TICK.fetch_add(1, Ordering::Relaxed) % 1024 == 1023 {
        writer.write(vec![
            WriteOp::Sql {
                sql: "DELETE FROM event WHERE time_created < ?1 AND time_created <> 0".into(),
                params: vec![(now - EVENT_MAX_AGE_MS).into()],
            },
            WriteOp::Sql {
                sql: "DELETE FROM event WHERE rowid IN (
                        SELECT rowid FROM (
                            SELECT rowid, ROW_NUMBER() OVER (
                                PARTITION BY session_id ORDER BY seq DESC
                            ) AS rn FROM event
                        ) WHERE rn > ?1
                    )"
                .into(),
                params: vec![EVENT_MAX_PER_SESSION.into()],
            },
        ])?;
    }
    Ok(())
}

/// Cheap existence check (opens a short-lived reader — M3 folds this into a pool).
pub fn session_exists(db: &std::path::Path, session_id: &str) -> anyhow::Result<bool> {
    let conn = pragma::open_reader(db)?;
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM session WHERE id = ?1",
        [session_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Insert a new session row (wire metadata shape) + publish-ready info.
pub fn insert_session(writer: &Writer, info: &serde_json::Value) -> anyhow::Result<()> {
    let id = info["id"].as_str().unwrap_or_default().to_string();
    writer.write(vec![WriteOp::Sql {
        sql: "INSERT OR REPLACE INTO session (id, project_id, slug, directory, path, title, version, \
              time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
            .into(),
        params: vec![
            id.into(),
            info["projectID"].as_str().unwrap_or("global").into(),
            info["slug"].as_str().unwrap_or("").into(),
            info["directory"].as_str().unwrap_or("").into(),
            info["path"].as_str().unwrap_or("").into(),
            info["title"].as_str().unwrap_or("").into(),
            info["version"].as_str().unwrap_or("").into(),
            info["time"]["created"].as_i64().unwrap_or(0).into(),
            info["time"]["updated"].as_i64().unwrap_or(0).into(),
        ],
    }])?;
    Ok(())
}

/// One session in upstream wire shape (list schema — always includes agent/
/// model/summary keys, null when unset; create-response is a leaner shape).
pub fn load_session_wire(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let conn = pragma::open_reader(db)?;
    let mut stmt = conn.prepare(
        "SELECT id, project_id, directory, path, slug, title, version, agent, model, cost,
                summary_additions, summary_deletions, summary_files,
                tokens_input, tokens_output, tokens_reasoning,
                tokens_cache_read, tokens_cache_write, time_created, time_updated
         FROM session WHERE id = ?1",
    )?;
    let rows = stmt.query_map([session_id], |r| {
        let model_txt: Option<String> = r.get(8)?;
        Ok(serde_json::json!({
            "id": r.get::<_, String>(0)?,
            "projectID": r.get::<_, String>(1)?,
            "directory": r.get::<_, String>(2)?,
            "path": r.get::<_, String>(3)?,
            "slug": r.get::<_, String>(4)?,
            "title": r.get::<_, String>(5)?,
            "version": r.get::<_, String>(6)?,
            "agent": r.get::<_, Option<String>>(7)?,
            "model": model_txt.as_deref().and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
                .unwrap_or(serde_json::Value::Null),
            "cost": r.get::<_, f64>(9)?,
            "summary": {"additions": r.get::<_, i64>(10)?, "deletions": r.get::<_, i64>(11)?, "files": r.get::<_, i64>(12)?},
            "tokens": {
                "input": r.get::<_, i64>(13)?,
                "output": r.get::<_, i64>(14)?,
                "reasoning": r.get::<_, i64>(15)?,
                "cache": {"read": r.get::<_, i64>(16)?, "write": r.get::<_, i64>(17)?},
            },
            "time": {"created": r.get::<_, i64>(18)?, "updated": r.get::<_, i64>(19)?},
        }))
    })?;
    Ok(rows.filter_map(|r| r.ok()).next())
}

/// Online backup (STORAGE §5): `VACUUM INTO` — consistent snapshot, source
/// untouched; restore = copy back + `PRAGMA integrity_check`.
pub fn backup_to(conn: &rusqlite::Connection, dest: &std::path::Path) -> anyhow::Result<()> {
    if dest.exists() {
        anyhow::bail!("backup destination exists: {}", dest.display());
    }
    let sql = format!("VACUUM INTO '{}'", dest.display());
    conn.execute_batch(&sql)?;
    Ok(())
}

// ---- oc-remote contract family (Batch 1: session/message/part mutations) ----

/// PATCH /session/{id} — title update (oc-remote rename sends {title} only;
/// metadata/permission/archived fields are not stored by refine — ignored).
pub fn update_session_title(
    writer: &Writer,
    session_id: &str,
    title: &str,
) -> anyhow::Result<usize> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    writer.write(vec![WriteOp::Sql {
        sql: "UPDATE session SET title = ?2, time_updated = ?3 WHERE id = ?1".into(),
        params: vec![session_id.into(), title.into(), now.into()],
    }])
}

/// DELETE /session/{id} — cascade messages/parts/todo (FKs), plus events
/// (no FK) and the session row. Blob GC is a separate pass (orphans are
/// expected until gc_orphans runs — STORAGE §3).
pub fn delete_session(
    writer: &Writer,
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<bool> {
    if !session_exists(db, session_id)? {
        return Ok(false);
    }
    writer.write(vec![
        WriteOp::Sql {
            sql: "DELETE FROM event WHERE session_id = ?1".into(),
            params: vec![session_id.into()],
        },
        WriteOp::Sql {
            sql: "DELETE FROM session WHERE id = ?1".into(),
            params: vec![session_id.into()],
        },
    ])?;
    Ok(true)
}

/// GET /session/{id}/children — sessions whose parent_id is this id (refine
/// never sets parent_id today → always []; shape-correct for the client).
pub fn load_children(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let conn = pragma::open_reader(db)?;
    let mut stmt =
        conn.prepare("SELECT id FROM session WHERE parent_id = ?1 ORDER BY time_updated DESC")?;
    let ids: Vec<String> = stmt
        .query_map([session_id], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    let mut out = Vec::new();
    for id in ids {
        if let Some(v) = load_session_wire(db, &id)? {
            out.push(v);
        }
    }
    Ok(out)
}

/// GET /session/{id}/todo — upstream Todo.Info subset oc-remote decodes:
/// {content, status, priority} (SseEvent.TodoUpdated.Todo).
pub fn load_todos(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let conn = pragma::open_reader(db)?;
    let mut stmt = conn.prepare(
        "SELECT content, status, priority FROM todo WHERE session_id = ?1 ORDER BY time_created",
    )?;
    let rows = stmt.query_map([session_id], |r| {
        Ok(serde_json::json!({
            "content": r.get::<_, String>(0)?,
            "status": r.get::<_, String>(1)?,
            "priority": r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        }))
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// DELETE /session/{id}/message/{mid} (parts cascade via FK).
pub fn delete_message(
    writer: &Writer,
    db: &std::path::Path,
    session_id: &str,
    message_id: &str,
) -> anyhow::Result<bool> {
    let exists: bool = {
        let conn = pragma::open_reader(db)?;
        conn.query_row(
            "SELECT count(*) FROM msg WHERE id = ?1 AND session_id = ?2",
            rusqlite::params![message_id, session_id],
            |r| r.get::<_, i64>(0).map(|n| n > 0),
        )?
    };
    if !exists {
        return Ok(false);
    }
    writer.write(vec![WriteOp::Sql {
        sql: "DELETE FROM msg WHERE id = ?1 AND session_id = ?2".into(),
        params: vec![message_id.into(), session_id.into()],
    }])?;
    Ok(true)
}

/// DELETE /session/{id}/message/{mid}/part/{pid}
pub fn delete_part(
    writer: &Writer,
    db: &std::path::Path,
    message_id: &str,
    part_id: &str,
) -> anyhow::Result<bool> {
    let exists: bool = {
        let conn = pragma::open_reader(db)?;
        conn.query_row(
            "SELECT count(*) FROM msg_part WHERE id = ?1 AND message_id = ?2",
            rusqlite::params![part_id, message_id],
            |r| r.get::<_, i64>(0).map(|n| n > 0),
        )?
    };
    if !exists {
        return Ok(false);
    }
    writer.write(vec![WriteOp::Sql {
        sql: "DELETE FROM msg_part WHERE id = ?1".into(),
        params: vec![part_id.into()],
    }])?;
    Ok(true)
}

/// PATCH .../part/{pid} — replace the stored part JSON (inline ≤8KB, else
/// blob). `data` is the client's full part object (native sessionID/messageID
/// keys — verified in oc-remote Part @SerialName).
pub fn update_part(
    writer: &Writer,
    blobs: &BlobStore,
    message_id: &str,
    part_id: &str,
    data: &serde_json::Value,
) -> anyhow::Result<bool> {
    let text = data.to_string();
    let byte_len = text.len() as i64;
    let (inline, sha) = if text.len() > INLINE_PART_MAX {
        let (sha, _, _) = blobs.put(text.as_bytes())?;
        (None, Some(sha))
    } else {
        (Some(text), None)
    };
    let n = writer.write(vec![WriteOp::Sql {
        sql: "UPDATE msg_part SET byte_len = ?3, inline = ?4, blob_sha = ?5 WHERE id = ?1 AND message_id = ?2"
            .into(),
        params: vec![
            part_id.into(),
            message_id.into(),
            byte_len.into(),
            inline.into(),
            sha.into(),
        ],
    }])?;
    Ok(n > 0)
}

/// End-of-prompt session row update (agent/model/cost/tokens/time).
/// Single-line SQL (a `\` line-continuation once produced a literal backslash
/// → syntax error → was swallowed by `let _ =` — AGENTS §2.5 forbids the
/// swallow; this fn propagates and is covered by a row-affecting test).
pub struct PromptStats<'a> {
    pub agent: &'a str,
    pub model_json: &'a str,
    pub cost: f64,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub time_updated: i64,
}

pub fn finalize_session_prompt(
    writer: &Writer,
    session_id: &str,
    stats: &PromptStats<'_>,
) -> anyhow::Result<usize> {
    writer.write(vec![WriteOp::Sql {
        sql: "UPDATE session SET agent = ?2, model = ?3, cost = ?4, tokens_input = ?5, tokens_output = ?6, tokens_cache_read = ?7, time_updated = ?8 WHERE id = ?1".into(),
        params: vec![
            session_id.into(),
            stats.agent.into(),
            stats.model_json.into(),
            stats.cost.into(),
            (stats.tokens_input as i64).into(),
            (stats.tokens_output as i64).into(),
            (stats.tokens_cache_read as i64).into(),
            stats.time_updated.into(),
        ],
    }])
}

/// Read-only opener for the LEGACY opencode database (delta sync source).
/// Deliberately NOT `pragma::open_reader`: that applies our connection
/// profile — this touches nothing but busy_timeout, and only because
/// READ_ONLY flags make every write attempt fail anyway. Never write here.
pub fn open_legacy_ro(path: &std::path::Path) -> anyhow::Result<rusqlite::Connection> {
    use anyhow::Context as _;
    let uri = format!(
        "file:{}?mode=ro",
        path.to_str()
            .context("legacy path utf-8")?
            .replace('?', "%3f")
    );
    let conn = rusqlite::Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_URI | rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .context("open legacy read-only")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(conn)
}

/// One legacy message with its parts (legacy row shapes; `data` verbatim).
pub struct LegacyMsg {
    pub id: String,
    pub time_created: i64,
    pub data: String,
    pub parts: Vec<LegacyPart>,
}

pub struct LegacyPart {
    pub id: String,
    pub time_created: i64,
    pub data: String,
}

/// Pull messages NEWER than the tuple cursor from the legacy DB, ordered
/// (time_created, id) ASC — upstream's own `message_session_time_created_id_idx`
/// seeks this; parts via `part_message_id_id_idx`. `cap` bounds one tick;
/// `has_more` (cap+1 probe) tells the caller to keep the cursor and continue
/// next tick. Tuple semantics match encode/decode_cursor (time, id).
pub fn pull_legacy_delta(
    conn: &rusqlite::Connection,
    session_id: &str,
    cursor: Option<(&str, i64)>,
    cap: u64,
) -> anyhow::Result<(Vec<LegacyMsg>, bool)> {
    let mut msgs: Vec<LegacyMsg> = match cursor {
        Some((cid, ctime)) => {
            let mut stmt = conn.prepare(
                "SELECT id, time_created, data FROM message \
                 WHERE session_id = ?1 AND (time_created > ?2 OR (time_created = ?2 AND id > ?3)) \
                 ORDER BY time_created, id LIMIT ?4",
            )?;
            stmt.query_map((session_id, ctime, cid, cap as i64 + 1), row_to_legacy)?
                .filter_map(|r| r.ok())
                .collect()
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, time_created, data FROM message \
                 WHERE session_id = ?1 ORDER BY time_created, id LIMIT ?2",
            )?;
            stmt.query_map((session_id, cap as i64 + 1), row_to_legacy)?
                .filter_map(|r| r.ok())
                .collect()
        }
    };
    let has_more = msgs.len() as u64 > cap;
    if has_more {
        msgs.truncate(cap as usize);
    }
    // parts for the window (one IN query; index-proven EXPLAIN in plan)
    if !msgs.is_empty() {
        let ph = (0..msgs.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT message_id, id, time_created, data FROM part \
             WHERE message_id IN ({ph}) ORDER BY message_id, time_created, id"
        ))?;
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(msgs.len());
        for m in &msgs {
            params.push(&m.id);
        }
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok((
                r.get::<_, String>(0)?,
                LegacyPart {
                    id: r.get(1)?,
                    time_created: r.get(2)?,
                    data: r.get(3)?,
                },
            ))
        })?;
        for row in rows.flatten() {
            if let Some(m) = msgs.iter_mut().find(|m| m.id == row.0) {
                m.parts.push(row.1);
            }
        }
    }
    Ok((msgs, has_more))
}

fn row_to_legacy(r: &rusqlite::Row<'_>) -> rusqlite::Result<LegacyMsg> {
    Ok(LegacyMsg {
        id: r.get(0)?,
        time_created: r.get(1)?,
        data: r.get(2)?,
        parts: Vec::new(),
    })
}

/// Count of session rows (sampler gauge; cheap integer scan).
pub fn session_count(db: &std::path::Path) -> anyhow::Result<i64> {
    let conn = pragma::open_reader(db)?;
    conn.query_row("SELECT count(*) FROM session", [], |r| r.get(0))
        .map_err(Into::into)
}

#[cfg(test)]
mod finalize_tests {
    use super::*;

    #[test]
    fn finalize_writes_agent_and_model_columns() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::writer::db_path(dir.path());
        let w = Writer::spawn(db.clone()).unwrap();
        w.write(vec![WriteOp::Sql {
            sql: "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_f', 'global', '/w', 's', 's', 't', '1', 1, 1)".into(),
            params: vec![],
        }])
        .unwrap();
        finalize_session_prompt(
            &w,
            "ses_f",
            &PromptStats {
                agent: "build",
                model_json: "{\"id\":\"m\",\"providerID\":\"p\",\"variant\":\"default\"}",
                cost: 0.5,
                tokens_input: 11,
                tokens_output: 22,
                tokens_cache_read: 33,
                time_updated: 1234,
            },
        )
        .expect("finalize must not fail (negative control: was silently swallowed)");
        drop(w); // join writer → flushed
        let conn = crate::pragma::open_reader(&db).unwrap();
        let (agent, model, tin, tcache): (String, String, i64, i64) = conn
            .query_row(
                "SELECT agent, model, tokens_input, tokens_cache_read FROM session WHERE id = 'ses_f'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(agent, "build");
        assert!(
            model.contains("\"providerID\":\"p\""),
            "model json: {model}"
        );
        assert_eq!(tin, 11);
        assert_eq!(tcache, 33);
    }
}
