//! POST /session/{id}/fork copy engine (freeze session.fork, session.ts:691).
//!
//! Upstream semantics mirrored exactly:
//! - `slice(0, findIndex(messageID))` → messages **strictly before** the cut
//!   message; no messageID → copy all; unknown messageID → findIndex -1 →
//!   copy all (freeze quirk, tested).
//! - `parentID` remapped through the old→new id map for assistant messages
//!   only; a parent outside the map keeps its ORIGINAL value (upstream
//!   `{...msg.info}` spread keeps the old id when `idMap.get` misses).
//! - compaction parts: `tail_start_id` remapped; a miss DROPS the key
//!   (upstream assigns `undefined`, JSON.stringify drops it).
//!
//! Divergence (intentional, documented in TRACEABILITY K-FORK):
//! - part content copies VERBATIM except compaction parts — column ids are
//!   authoritative at serve time (`merge_columns`, M1) and content ids have
//!   no readers (grep-proven). Verbatim content ⇒ **blob parts share the same
//!   sha** (zero-copy fork; a 100 MB part costs a row, not 100 MB).
//! - no `message.updated`/`message.part.updated` SSE during the copy
//!   (upstream fires them from updateMessage/updatePart; every known consumer
//!   — dialog-fork, ACP forkSession, oc-remote ChatViewModel — REST-loads the
//!   new session after the HTTP response; flooding full-part frames through
//!   the byte-bounded ring would evict other sessions' live events, S5).
//! - rollback on mid-copy failure (delete_session → FK cascade) — upstream
//!   leaves partial rows behind.
//!
//! All `msg_part` writes stay in this crate (check-guards rule 4).

use crate::{BlobStore, WriteOp, Writer, pragma};
use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use serde_json::Value;

/// Result of a fork copy.
#[derive(Debug, Clone, Copy, Default)]
pub struct ForkStats {
    pub messages: u64,
    pub parts: u64,
}

/// I/O context for a fork: writer, blob store, source db (bundled — the
/// bare parameter list trips `clippy::too_many_arguments` and every caller
/// passes the same trio from `AppState`).
#[derive(Clone, Copy)]
pub struct ForkEnv<'a> {
    pub writer: &'a Writer,
    pub blobs: Option<&'a BlobStore>,
    pub db: &'a std::path::Path,
}

/// Ops flush granularity: bounded memory on huge sessions (M3 streaming
/// discipline — never materialize a whole session's rows in one batch).
const FORK_CHUNK_OPS: usize = 512;

/// Copy `src_id`'s messages (chronological `(time_created, id)` order — the
/// canonical tuple) into the already-built destination session info.
/// The session row is inserted first (FK), messages/parts follow in chunked
/// writer batches; any failure rolls the whole destination back
/// (delete_session → FK cascade).
///
/// `mint_msg`/`mint_part` generate fresh ids (ocserve-core owns id format —
/// this crate must not depend upward).
pub fn fork_session(
    env: &ForkEnv,
    dst_info: &Value,
    src_id: &str,
    upto: Option<&str>,
    mut mint_msg: impl FnMut() -> String,
    mut mint_part: impl FnMut() -> String,
) -> Result<ForkStats> {
    let dst_id = dst_info["id"]
        .as_str()
        .context("fork: dst_info missing id")?;
    crate::insert_session(env.writer, dst_info).context("fork: insert dst session")?;
    match fork_messages(env, dst_id, src_id, upto, &mut mint_msg, &mut mint_part) {
        Ok(stats) => Ok(stats),
        Err(e) => {
            // partial copy must never surface: a failed fork 500s with no
            // session (observable-equivalent to upstream's invisible half-row
            // state, minus the corruption).
            if !crate::session_exists(env.db, dst_id).unwrap_or(false) {
                return Err(e); // session row never landed
            }
            if let Err(del) = crate::delete_session(env.writer, env.db, dst_id) {
                tracing::error!("fork rollback failed: {del:#}");
            }
            Err(e)
        }
    }
}

fn fork_messages(
    env: &ForkEnv,
    dst_id: &str,
    src_id: &str,
    upto: Option<&str>,
    mint_msg: &mut dyn FnMut() -> String,
    mint_part: &mut dyn FnMut() -> String,
) -> Result<ForkStats> {
    let ForkEnv { writer, blobs, db } = *env;
    let conn = pragma::open_reader(db)?;
    // ids only (bounded: ~30 B/message — the no-materialization guard targets
    // info+parts+blobs, not a 500 KB id list)
    let ids: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT id FROM msg WHERE session_id = ?1 ORDER BY time_created, id")?;
        let rows = stmt.query_map([src_id], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let mut info_stmt = conn.prepare("SELECT role, time_created, info FROM msg WHERE id = ?1")?;
    let mut part_stmt = conn.prepare(
        "SELECT type, byte_len, inline, blob_sha FROM msg_part \
         WHERE message_id = ?1 ORDER BY seq",
    )?;

    let mut stats = ForkStats::default();
    let mut id_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut ops: Vec<WriteOp> = Vec::new();
    let mut ops_est = 0usize;

    for src_mid in ids {
        if upto == Some(src_mid.as_str()) {
            break; // slice(0, findIndex): strictly before the cut
        }
        let Some((role, time_created, info_txt)) = info_stmt
            .query_row([src_mid.as_str()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .optional()?
        else {
            continue; // deleted between listing and read — skip (fork keeps going)
        };
        let mut info: Value = serde_json::from_str(&info_txt).context("fork: msg info JSON")?;
        let new_mid = mint_msg();
        info["id"] = Value::String(new_mid.clone());
        info["sessionID"] = Value::String(dst_id.to_string());
        // freeze: remap ONLY assistant messages whose parent is in the map;
        // a missing parent keeps the original value (spread pass-through)
        if role == "assistant"
            && let Some(p) = info.get("parentID").and_then(|v| v.as_str())
            && let Some(np) = id_map.get(p)
        {
            info["parentID"] = Value::String(np.clone());
        }
        ops.push(WriteOp::Sql {
            sql: "INSERT OR REPLACE INTO msg (id, session_id, role, seq, time_created, info) \
                  VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(seq),0)+1 FROM msg WHERE session_id=?2), ?4, ?5)"
                .into(),
            params: vec![
                new_mid.clone().into(),
                dst_id.into(),
                role.clone().into(),
                time_created.into(),
                info.to_string().into(),
            ],
        });
        ops_est += 1;

        // summary assistants link their anchor (M6 completedCompactions)
        if info["summary"] == Value::Bool(true)
            && let Some(parent) = info.get("parentID").and_then(|v| v.as_str())
        {
            ops.push(WriteOp::Sql {
                sql: "UPDATE compaction SET summary_msg_id = ?2 \
                      WHERE user_msg_id = ?1 AND summary_msg_id IS NULL"
                    .into(),
                params: vec![parent.into(), new_mid.as_str().into()],
            });
            ops_est += 1;
        }

        let parts = part_stmt
            .query_map([src_mid.as_str()], |r| {
                Ok((
                    r.get::<_, String>(0)?,         // type
                    r.get::<_, i64>(1)?,            // byte_len
                    r.get::<_, Option<String>>(2)?, // inline
                    r.get::<_, Option<String>>(3)?, // blob_sha
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (ptype, byte_len, inline, blob_sha) in parts {
            // orphan rows (no inline, no sha) never serve (for_each skips
            // them) — skip them here too, matching the read path
            if inline.is_none() && blob_sha.is_none() {
                continue;
            }
            let new_prt = mint_part();
            // content: compaction parts carry the functional tail_start_id →
            // rewrite; everything else copies VERBATIM (zero-copy blobs).
            // Search projection text = the stored content in both cases.
            let (text_opt, out_inline, out_sha, out_len): (
                Option<String>,
                Option<String>,
                Option<String>,
                i64,
            ) = if ptype == "compaction" {
                let raw = match (&inline, &blob_sha) {
                    (Some(t), _) => t.clone(),
                    (None, Some(sha)) => {
                        let bytes = blobs
                            .context("fork: compaction part is blobbed but no blob store")?
                            .get(sha, byte_len as u64)
                            .with_context(|| format!("fork: read blob {sha}"))?;
                        String::from_utf8_lossy(&bytes).into_owned()
                    }
                    (None, None) => unreachable!("orphan checked above"),
                };
                let mut v: Value = serde_json::from_str(&raw).context("fork: compaction JSON")?;
                v["id"] = Value::String(new_prt.clone());
                v["sessionID"] = Value::String(dst_id.to_string());
                v["messageID"] = Value::String(new_mid.clone());
                if let Some(t) = v.get("tail_start_id").and_then(|x| x.as_str()) {
                    match id_map.get(t) {
                        Some(nt) => v["tail_start_id"] = Value::String(nt.clone()),
                        None => {
                            v.as_object_mut().map(|o| o.remove("tail_start_id"));
                        }
                    }
                }
                let text = v.to_string();
                let len = text.len() as i64;
                if len > crate::INLINE_PART_MAX as i64
                    && let Some(store) = blobs
                {
                    let (sha, _, _) = store.put(text.as_bytes())?;
                    (Some(text.clone()), None, Some(sha), len)
                } else {
                    let inline_out = Some(text.clone());
                    (Some(text), inline_out, None, len)
                }
            } else {
                // verbatim: inline stays inline; blob keeps its sha (shared)
                let text = match (&inline, &blob_sha) {
                    (Some(t), _) => Some(t.clone()),
                    (None, Some(sha)) => {
                        let bytes = blobs
                            .context("fork: blobbed part but no blob store")?
                            .get(sha, byte_len as u64)
                            .with_context(|| format!("fork: read blob {sha}"))?;
                        Some(String::from_utf8_lossy(&bytes).into_owned())
                    }
                    (None, None) => unreachable!("orphan checked above"),
                };
                (text, inline.clone(), blob_sha.clone(), byte_len)
            };

            ops.push(WriteOp::Sql {
                sql: "INSERT OR REPLACE INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) \
                      VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(seq),0)+1 FROM msg_part WHERE message_id=?2), ?4, ?5, ?6, ?7)"
                    .into(),
                params: vec![
                    new_prt.clone().into(),
                    new_mid.clone().into(),
                    dst_id.into(),
                    ptype.clone().into(),
                    out_len.into(),
                    out_inline.into(),
                    out_sha.into(),
                ],
            });
            ops_est += 2;
            if let Some(text) = &text_opt {
                ops.push(crate::part_search_upsert_ops(
                    &new_prt, dst_id, &new_mid, text,
                ));
                if ptype == "compaction" {
                    ops.push(crate::compaction_upsert_ops(
                        &new_prt, dst_id, &new_mid, text,
                    ));
                }
                ops_est += 1;
            }
            stats.parts += 1;
        }

        id_map.insert(src_mid, new_mid);
        stats.messages += 1;

        if ops_est >= FORK_CHUNK_OPS {
            writer
                .write(std::mem::take(&mut ops))
                .context("fork: writer chunk")?;
            ops_est = 0;
        }
    }
    if !ops.is_empty() {
        writer.write(ops).context("fork: writer final chunk")?;
    }
    Ok(stats)
}
