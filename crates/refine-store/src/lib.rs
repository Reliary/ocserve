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
        let mut pstmt = conn.prepare(
            "SELECT inline, blob_sha, byte_len FROM msg_part WHERE message_id = ?1 ORDER BY seq",
        )?;
        let mut parts: Vec<serde_json::Value> = Vec::new();
        let mut rows = pstmt.query([&mid])?;
        while let Some(row) = rows.next()? {
            let inline: Option<String> = row.get(0)?;
            let sha: Option<String> = row.get(1)?;
            let byte_len: i64 = row.get(2)?;
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
                parts.push(v);
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

/// Persist a durable event (bounded ring — STORAGE §4 retention applies later).
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
