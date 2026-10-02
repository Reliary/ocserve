//! Legacy→refine delta sync (development bridge; user: "development
//! machinery until the final migration"). Additive-only: pulls messages/
//! parts for sessions that exist in BOTH databases, emits live SSE so an
//! open oc-remote view updates, refreshes session title/time (legacy wins
//! for imported sessions — documented divergence, TESTING §1.6).
//!
//! Safety: legacy is opened READ-ONLY (store::open_legacy_ro), fail-soft
//! (missing/locked legacy → Err for the caller to log, never panics), kill
//! switch `REFINE_LEGACY_SYNC=0`, path override `REFINE_LEGACY_DB`.

use crate::{AppState, emit_durable_http};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

pub const SESSION_CAP: u64 = 500;
pub const GLOBAL_CAP: u64 = 2000;

#[derive(Debug, Default, Clone, Copy)]
pub struct SyncStats {
    pub messages: u64,
    pub parts: u64,
    pub sessions: u64,
    /// more work remained after the per-session/global caps
    pub backlog: bool,
}

pub fn sync_enabled() -> bool {
    std::env::var("REFINE_LEGACY_SYNC")
        .map(|v| v != "0")
        .unwrap_or(true)
}

pub fn legacy_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("REFINE_LEGACY_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(".local/share/opencode/opencode.db")
}

/// One sync pass (env-configured source). `Err` = source unavailable
/// (caller logs + error metric).
pub fn sync_tick(st: &Arc<AppState>) -> Result<SyncStats> {
    sync_tick_at(st, &legacy_db_path())
}

/// One sync pass against an explicit source path (tests use this — no env).
pub fn sync_tick_at(st: &Arc<AppState>, legacy_path: &std::path::Path) -> Result<SyncStats> {
    if !legacy_path.exists() {
        anyhow::bail!("legacy db not found at {}", legacy_path.display());
    }
    let legacy = refine_store::open_legacy_ro(legacy_path)?;
    // Attach our own DB read-only so hole detection can anti-join against
    // live refine rows (never writes; both sides read-only).
    let refine_uri = format!(
        "file:{}?mode=ro",
        st.db.to_str().unwrap_or_default().replace('?', "%3f")
    );
    legacy
        .execute("ATTACH DATABASE ? AS refine_ro", [&refine_uri])
        .context("attach refine for hole check")?;

    // refine's sessions ∩ legacy's sessions = the imported (adopted) set
    let refine_ids: Vec<String> = {
        let conn = refine_store::pragma::open_reader(&st.db)?;
        let mut stmt = conn.prepare("SELECT id FROM session ORDER BY id")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>()
    };
    if refine_ids.is_empty() {
        return Ok(SyncStats::default());
    }
    let ph = (1..=refine_ids.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut legacy_sessions: std::collections::HashMap<String, (String, i64)> =
        std::collections::HashMap::new();
    {
        let mut stmt = legacy.prepare(&format!(
            "SELECT id, title, time_updated FROM session WHERE id IN ({ph})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(refine_ids.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows.flatten() {
            legacy_sessions.insert(row.0, (row.1, row.2));
        }
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // adoption: imported sessions get a sync row seeded at refine's newest
    // message (first pull only brings the legacy-side gap — not a re-import)
    let mut to_adopt: Vec<(String, Option<String>)> = Vec::new();
    for sid in legacy_sessions.keys() {
        let conn = refine_store::pragma::open_reader(&st.db)?;
        let exists: i64 = conn.query_row(
            "SELECT count(*) FROM import_sync WHERE session_id = ?1",
            [sid.as_str()],
            |r| r.get(0),
        )?;
        if exists > 0 {
            continue;
        }
        let newest: Option<(String, i64)> = {
            let mut stmt =
                conn.prepare("SELECT id, time_created FROM msg WHERE session_id = ?1 ORDER BY time_created DESC, id DESC LIMIT 1")?;
            stmt.query_row([sid.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
                .ok()
        };
        let cursor = newest.map(|(id, t)| refine_store::encode_cursor(&id, t));
        to_adopt.push((sid.clone(), cursor));
    }
    if !to_adopt.is_empty() {
        let mut ops = Vec::new();
        for (sid, cursor) in &to_adopt {
            ops.push(refine_store::WriteOp::Sql {
                sql: "INSERT OR IGNORE INTO import_sync (session_id, cursor, last_sync_ms) VALUES (?1, ?2, ?3)".into(),
                params: vec![sid.clone().into(), cursor.clone().into(), now_ms.into()],
            });
        }
        st.writer.write(ops)?;
    }

    // sync rows (adopted set)
    let rows: Vec<(String, Option<String>)> = {
        let conn = refine_store::pragma::open_reader(&st.db)?;
        let mut stmt =
            conn.prepare("SELECT session_id, cursor FROM import_sync ORDER BY session_id")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect()
    };

    let mut stats = SyncStats::default();
    let mut budget = GLOBAL_CAP;
    'sessions: for (sid, cursor_txt) in rows {
        if budget == 0 {
            stats.backlog = true;
            break;
        }
        let mut cursor_owned = cursor_txt
            .as_deref()
            .map(refine_store::decode_cursor)
            .transpose()?;
        // Clamp to the newest COMMON message: refine may hold its own later
        // sends (user typed from the :4912 view) while legacy's in-between
        // messages never landed — adoption-at-refine-newest then skips that
        // hole forever (live bug 2026-10-02, fixed here). Cursor only moves
        // BACK to the newest legacy-id-in-refine point; pulls then fill the
        // gap and the cursor walks forward again (self-healing per tick).
        // Hole repair (live bug 2026-10-02): a refine-side send can sit
        // AHEAD of legacy messages that never landed, and a cursor adopted/
        // walked past them skips that region forever. Find the OLDEST legacy
        // message missing from refine; rewind the cursor to its legacy-order
        // predecessor (or the beginning). Re-pulls are INSERT-OR-IGNORE +
        // existing-filtered, so the tail costs nothing.
        let oldest_missing: Option<(String, i64)> = legacy
            .query_row(
                "SELECT m.id, m.time_created FROM message m \
                 WHERE m.session_id = ?1 AND NOT EXISTS (\
                    SELECT 1 FROM refine_ro.msg r WHERE r.id = m.id) \
                 ORDER BY m.time_created, m.id LIMIT 1",
                [&sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if let Some((mid, mt)) = oldest_missing {
            let pred: Option<(String, i64)> = legacy
                .query_row(
                    "SELECT id, time_created FROM message \
                     WHERE session_id = ?1 AND (time_created < ?2 OR (time_created = ?2 AND id < ?3)) \
                     ORDER BY time_created DESC, id DESC LIMIT 1",
                    rusqlite::params![sid, mt, mid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            match pred {
                Some(pred) => {
                    let ahead = cursor_owned.as_ref().is_none_or(|(cid, ct)| {
                        *ct > pred.1 || (*ct == pred.1 && cid.as_str() > pred.0.as_str())
                    });
                    if ahead {
                        tracing::info!(
                            "sync cursor rewound for hole session={sid}: -> t{} (missing {mid})",
                            pred.1
                        );
                        cursor_owned = Some(pred);
                    }
                }
                None => {
                    // hole starts at the beginning → full re-walk
                    if cursor_owned.is_some() {
                        tracing::info!("sync cursor rewound to start session={sid}");
                        cursor_owned = None;
                    }
                }
            }
        }
        let cursor = cursor_owned.as_ref().map(|(id, t)| (id.as_str(), *t));
        let cap = SESSION_CAP.min(budget);
        let (msgs, has_more) = refine_store::pull_legacy_delta(&legacy, &sid, cursor, cap)?;
        if msgs.is_empty() {
            continue;
        }
        stats.sessions += 1;
        if has_more {
            stats.backlog = true;
        }

        // ids ALREADY in refine (idempotent re-pull after a crash: skip them)
        let existing_ids: std::collections::HashSet<String> = {
            let conn = refine_store::pragma::open_reader(&st.db)?;
            let mut set = std::collections::HashSet::new();
            for chunk in msgs.chunks(100) {
                let ph = (1..=chunk.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(",");
                let mut stmt = conn.prepare(&format!("SELECT id FROM msg WHERE id IN ({ph})"))?;
                let found = stmt
                    .query_map(
                        rusqlite::params_from_iter(chunk.iter().map(|m| m.id.as_str())),
                        |r| r.get::<_, String>(0),
                    )?
                    .filter_map(|r| r.ok())
                    .collect::<std::collections::HashSet<_>>();
                for id in found {
                    set.insert(id);
                }
            }
            set
        };

        let mut seq: i64 = {
            let conn = refine_store::pragma::open_reader(&st.db)?;
            conn.query_row(
                "SELECT COALESCE(MAX(seq),0) FROM msg WHERE session_id = ?1",
                [&sid],
                |r| r.get(0),
            )?
        };

        let mut last: Option<(String, i64)> = None;
        for m in &msgs {
            if budget == 0 {
                stats.backlog = true;
                break 'sessions;
            }
            budget -= 1;
            last = Some((m.id.clone(), m.time_created));
            if existing_ids.contains(&m.id) {
                continue; // already landed (crash-retry path)
            }
            seq += 1;
            let data_v: serde_json::Value =
                serde_json::from_str(&m.data).unwrap_or(serde_json::Value::Null);
            let role = data_v["role"].as_str().unwrap_or("user");
            let mut info = data_v.clone();
            info["id"] = serde_json::Value::String(m.id.clone());
            info["sessionID"] = serde_json::Value::String(sid.clone());

            let mut ops = vec![refine_store::WriteOp::Sql {
                sql: "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES (?1, ?2, ?3, ?4, ?5, ?6)".into(),
                params: vec![
                    m.id.clone().into(),
                    sid.clone().into(),
                    role.to_string().into(),
                    seq.into(),
                    m.time_created.into(),
                    info.to_string().into(),
                ],
            }];
            let mut part_seq: i64 = 0;
            let mut merged_parts: Vec<serde_json::Value> = Vec::new();
            for p in &m.parts {
                part_seq += 1;
                let mut pv: serde_json::Value =
                    serde_json::from_str(&p.data).unwrap_or(serde_json::Value::Null);
                pv["id"] = serde_json::Value::String(p.id.clone());
                pv["sessionID"] = serde_json::Value::String(sid.clone());
                pv["messageID"] = serde_json::Value::String(m.id.clone());
                let ptype = pv["type"].as_str().unwrap_or("text");
                let (inline, sha): (Option<String>, Option<String>) =
                    if p.data.len() > refine_store::INLINE_PART_MAX {
                        (None, Some(st.blobs.put(p.data.as_bytes())?.0))
                    } else {
                        (Some(p.data.clone()), None)
                    };
                ops.push(refine_store::WriteOp::Sql {
                    sql: "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)".into(),
                    params: vec![
                        p.id.clone().into(),
                        m.id.clone().into(),
                        sid.clone().into(),
                        part_seq.into(),
                        ptype.to_string().into(),
                        (p.data.len() as i64).into(),
                        inline.into(),
                        sha.into(),
                    ],
                });
                merged_parts.push(pv);
                stats.parts += 1;
            }
            // session title/time refreshed from legacy (legacy wins — documented)
            if let Some((title, t_upd)) = legacy_sessions.get(&sid) {
                ops.push(refine_store::WriteOp::Sql {
                    sql: "UPDATE session SET title = ?2, time_updated = ?3 WHERE id = ?1".into(),
                    params: vec![sid.clone().into(), title.clone().into(), (*t_upd).into()],
                });
            }
            st.writer.write(ops)?;

            // live SSE: reducer merges these into any open view of this session
            emit_durable_http(
                st,
                &sid,
                "message.updated",
                serde_json::json!({
                    "sessionID": sid,
                    "info": info,
                }),
            )
            .map_err(|e| anyhow::anyhow!("emit message.updated: {} {}", e.name, e.message))?;
            for pv in merged_parts {
                emit_durable_http(
                    st,
                    &sid,
                    "message.part.updated",
                    serde_json::json!({
                        "sessionID": sid,
                        "part": pv,
                    }),
                )
                .map_err(|e| anyhow::anyhow!("emit part.updated: {} {}", e.name, e.message))?;
            }
            stats.messages += 1;
        }

        if let Some((id, t)) = last {
            let cur = refine_store::encode_cursor(&id, t);
            st.writer.write(vec![refine_store::WriteOp::Sql {
                sql: "UPDATE import_sync SET cursor = ?2, last_sync_ms = ?3 WHERE session_id = ?1"
                    .into(),
                params: vec![sid.clone().into(), cur.into(), now_ms.into()],
            }])?;
        }
    }
    Ok(stats)
}
