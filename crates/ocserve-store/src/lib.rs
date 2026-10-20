//! ocserve-store: SQLite (pragmas/schema/writer) + chunked blob store.
//!
//! Invariants (see STORAGE.md / MEMORY.md / AGENTS.md):
//! - PRAGMAs only via `pragma::{create_new,open_writer,open_reader}`
//! - no connection held across `.await` (enforced by the scoped `Writer` API)
//! - payloads via `blob::BlobStore`, never as SQLite row payloads > metadata
#![cfg_attr(not(test), forbid(unsafe_code))]

use rusqlite::OptionalExtension;

pub mod blob;
pub mod fork;
pub mod platform;
pub mod pragma;
pub mod schema;
pub mod writer;

pub use blob::BlobStore;
pub use fork::{ForkStats, fork_session};
pub use writer::{WriteOp, Writer, apply_ops};

/// Upstream wire shape of a session list entry (PLAN F1; keys verified against
/// recorded manifest session_list keys).
pub fn load_sessions_wire(db: &std::path::Path) -> anyhow::Result<Vec<serde_json::Value>> {
    // F8 memo: single slot, epoch-exact (see F7/F8 module). Lock is NEVER
    // held across the query — the writer's eager clear shares this lock.
    let enabled = list_memo_enabled();
    let e0 = write_epoch();
    if enabled {
        let hit = {
            let mut l = list_memo().lock();
            // epoch AND db identity (see ListMemoState::db): a foreign db
            // clears the slot rather than being served it.
            if l.db.as_deref().is_some_and(|d| d != db) {
                l.slot = None;
                l.bytes = None;
                l.db = None;
            }
            if l.epoch == e0 { l.slot.clone() } else { None }
        };
        if let Some(v) = hit {
            LIST_MEMO_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok((*v).clone());
        }
        LIST_MEMO_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let conn = pragma::open_reader(db)?;
    let mut stmt = conn.prepare_cached(
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
    if enabled && write_epoch() == e0 {
        let mut l = list_memo().lock();
        l.epoch = e0;
        l.db = Some(db.to_path_buf());
        l.slot = Some(std::sync::Arc::new(out.clone()));
    }
    Ok(out)
}

/// F5-extends-F8: same list, serialized ONCE per epoch. GET /session
/// serves these bytes refcounted (zero serde on the hit path). Same kill
/// switch (OCSERVE_LIST_MEMO=0) — a cache-off run disables both layers.
pub fn load_sessions_wire_bytes(
    db: &std::path::Path,
) -> anyhow::Result<std::sync::Arc<bytes::Bytes>> {
    let enabled = list_memo_enabled();
    let e0 = write_epoch();
    if enabled {
        let hit = {
            let mut l = list_memo().lock();
            if l.db.as_deref().is_some_and(|d| d != db) {
                l.slot = None;
                l.bytes = None;
                l.db = None;
            }
            if l.epoch == e0 { l.bytes.clone() } else { None }
        };
        if let Some(b) = hit {
            return Ok(b);
        }
    }
    // M1: serialize the wire body DIRECTLY from the columns into one buffer
    // — no `Value` tree per session and no second serialization pass. The
    // Value-slot memo is left to `load_sessions_wire` (its own callers keep
    // their path); this function's callers only ever want bytes, so building
    // both would double the work. Measured on the fixture: 201 sessions cost
    // 684 µs via the DOM path, the most expensive store query after the page.
    let vec = build_sessions_wire_bytes(db)?;
    if enabled && write_epoch() == e0 {
        let mut l = list_memo().lock();
        l.epoch = e0;
        l.db = Some(db.to_path_buf());
        l.bytes = Some(std::sync::Arc::new(vec.clone()));
        // The Value slot is intentionally NOT populated here: filling it
        // would run the DOM path this function exists to avoid. Callers that
        // want `Vec<Value>` call `load_sessions_wire`, which fills it.
    }
    Ok(std::sync::Arc::new(vec))
}

/// W4: session-list query (freeze ListQuery parity, handlers/session.ts +
/// session.ts listByProject): limit defaults 100, `roots` filters
/// `parent_id IS NULL`, `search` is a title LIKE, `start` a time_updated
/// floor, `directory` an exact match unless `scope=project`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionListQuery {
    pub limit: Option<usize>,
    pub roots: bool,
    pub search: Option<String>,
    pub start: Option<i64>,
    pub directory: Option<String>,
    /// scope=project ⇒ ignore `directory` (freeze `scope !== "project"` gate).
    pub scope_project: bool,
}

impl SessionListQuery {
    /// True when the query is the bare list (no filters, no explicit limit).
    /// Only this form uses the single-slot memo + the exact legacy bytes.
    pub fn is_default(&self) -> bool {
        self.limit.is_none()
            && !self.roots
            && self.search.is_none()
            && self.start.is_none()
            && self.directory.is_none()
    }

    /// Canonical memo key.
    pub fn key(&self) -> String {
        format!(
            "l={:?}|r={}|s={:?}|t={:?}|d={:?}|p={}",
            self.limit, self.roots, self.search, self.start, self.directory, self.scope_project
        )
    }
}

/// W4: parameterized session list → wire bytes. Freeze semantics + order
/// (time_updated DESC). Empty result is `[]`.
pub fn load_sessions_wire_bytes_filtered(
    db: &std::path::Path,
    q: &SessionListQuery,
) -> anyhow::Result<std::sync::Arc<bytes::Bytes>> {
    if q.is_default() {
        return load_sessions_wire_bytes(db);
    }
    let enabled = list_memo_enabled();
    let e0 = write_epoch();
    if enabled {
        let hit = {
            let mut l = filtered_memo().lock();
            l.get(&q.key(), e0, db)
        };
        if let Some(b) = hit {
            return Ok(b);
        }
    }
    let bytes = std::sync::Arc::new(build_sessions_wire_bytes_filtered(db, q)?);
    if enabled && write_epoch() == e0 {
        filtered_memo().lock().put(q.key(), bytes.clone(), e0, db);
    }
    Ok(bytes)
}

/// M1: the session-list wire body, serialized straight from the columns.
///
/// Byte-for-byte what `serde_json::to_vec(&load_sessions_wire(db)?)` emits —
/// pinned by `tests/list_wire_parity.rs`, which asserts equality against the
/// DOM path over a fixture with populated, NULL and unusual columns. Member
/// order follows the `json!` literal in `load_sessions_wire` because
/// `serde_json` is built with `preserve_order` (insertion order).
///
/// Every number goes through serde_json itself (`Value::from(f64)` /
/// `Value::from(i64)` then `to_writer`) rather than a hand-rolled formatter,
/// so ryu formatting is serde's by construction instead of by coincidence.
fn build_sessions_wire_bytes(db: &std::path::Path) -> anyhow::Result<bytes::Bytes> {
    let conn = pragma::open_reader(db)?;
    let mut stmt = conn.prepare_cached(
        "SELECT id, project_id, directory, path, slug, title, version, agent, model, cost,
                summary_additions, summary_deletions, summary_files,
                tokens_input, tokens_output, tokens_reasoning,
                tokens_cache_read, tokens_cache_write, time_created, time_updated
         FROM session ORDER BY time_updated DESC",
    )?;
    write_session_rows(&mut stmt, &[])
}

/// W4: filtered list. Same column set + row serializer as the unfiltered
/// path (byte parity by construction); filters pushed into SQL per freeze
/// listByProject. `limit` defaults to 100 (freeze). Order time_updated DESC.
fn build_sessions_wire_bytes_filtered(
    db: &std::path::Path,
    q: &SessionListQuery,
) -> anyhow::Result<bytes::Bytes> {
    let conn = pragma::open_reader(db)?;
    let mut where_clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if q.roots {
        where_clauses.push("parent_id IS NULL".into());
    }
    if let Some(d) = &q.directory
        && !q.scope_project
    {
        where_clauses.push("directory = ?".into());
        params.push(Box::new(d.clone()));
    }
    if let Some(st) = q.start {
        where_clauses.push("time_updated >= ?".into());
        params.push(Box::new(st));
    }
    if let Some(se) = &q.search {
        // freeze: like(title, %search%) — case-insensitive ASCII by SQLite
        where_clauses.push("title LIKE ?".into());
        params.push(Box::new(format!("%{se}%")));
    }
    let limit = q.limit.unwrap_or(100);
    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT id, project_id, directory, path, slug, title, version, agent, model, cost, \
         summary_additions, summary_deletions, summary_files, \
         tokens_input, tokens_output, tokens_reasoning, \
         tokens_cache_read, tokens_cache_write, time_created, time_updated \
         FROM session{where_sql} ORDER BY time_updated DESC LIMIT ?"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    params.push(Box::new(limit as i64));
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
    write_session_rows(&mut stmt, &refs)
}

/// Shared row serializer: identical column order + wire format as the
/// unfiltered path (M1 contract). Takes any prepared list statement whose
/// projection matches the 20 columns above.
fn write_session_rows(
    stmt: &mut rusqlite::CachedStatement<'_>,
    params: &[&dyn rusqlite::ToSql],
) -> anyhow::Result<bytes::Bytes> {
    let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
    out.push(b'[');
    let mut first = true;
    let mut rows = stmt.query(params)?;
    while let Some(row) = rows.next()? {
        if !first {
            out.push(b',');
        }
        first = false;
        out.extend_from_slice(b"{\"id\":");
        push_wire_str(&mut out, &row.get::<_, String>(0)?);
        out.extend_from_slice(b",\"projectID\":");
        push_wire_str(&mut out, &row.get::<_, String>(1)?);
        out.extend_from_slice(b",\"directory\":");
        push_wire_str(&mut out, &row.get::<_, String>(2)?);
        out.extend_from_slice(b",\"path\":");
        push_wire_str(&mut out, &row.get::<_, String>(3)?);
        out.extend_from_slice(b",\"slug\":");
        push_wire_str(&mut out, &row.get::<_, String>(4)?);
        out.extend_from_slice(b",\"title\":");
        push_wire_str(&mut out, &row.get::<_, String>(5)?);
        out.extend_from_slice(b",\"version\":");
        push_wire_str(&mut out, &row.get::<_, String>(6)?);
        out.extend_from_slice(b",\"agent\":");
        match row.get::<_, Option<String>>(7)? {
            Some(a) => push_wire_str(&mut out, &a),
            None => out.extend_from_slice(b"null"),
        }
        // `model` is itself JSON text in the column; re-emit through serde so
        // a stored blob with non-compact formatting normalizes as before
        out.extend_from_slice(b",\"model\":");
        let model_txt: Option<String> = row.get(8)?;
        match model_txt
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
        {
            Some(Ok(v)) => serde_json::to_writer(&mut out, &v)?,
            _ => {
                out.extend_from_slice(b"{\"id\":\"\",\"providerID\":\"\",\"variant\":\"default\"}")
            }
        }
        out.extend_from_slice(b",\"cost\":");
        let cost: f64 = row.get(9)?;
        push_wire_f64(&mut out, cost);
        out.extend_from_slice(b",\"summary\":{\"additions\":");
        push_wire_i64(&mut out, row.get::<_, i64>(10)?);
        out.extend_from_slice(b",\"deletions\":");
        push_wire_i64(&mut out, row.get::<_, i64>(11)?);
        out.extend_from_slice(b",\"files\":");
        push_wire_i64(&mut out, row.get::<_, i64>(12)?);
        out.extend_from_slice(b"},\"tokens\":{\"input\":");
        push_wire_i64(&mut out, row.get::<_, i64>(13)?);
        out.extend_from_slice(b",\"output\":");
        push_wire_i64(&mut out, row.get::<_, i64>(14)?);
        out.extend_from_slice(b",\"reasoning\":");
        push_wire_i64(&mut out, row.get::<_, i64>(15)?);
        out.extend_from_slice(b",\"cache\":{\"read\":");
        push_wire_i64(&mut out, row.get::<_, i64>(16)?);
        out.extend_from_slice(b",\"write\":");
        push_wire_i64(&mut out, row.get::<_, i64>(17)?);
        out.extend_from_slice(b"}},\"time\":{\"created\":");
        push_wire_i64(&mut out, row.get::<_, i64>(18)?);
        out.extend_from_slice(b",\"updated\":");
        push_wire_i64(&mut out, row.get::<_, i64>(19)?);
        out.extend_from_slice(b"}}");
    }
    out.push(b']');
    Ok(bytes::Bytes::from(out))
}

/// JSON string escaping identical to serde's for a string value — one
/// implementation, shared with the splice module.
fn push_wire_str(out: &mut Vec<u8>, s: &str) {
    crate::splice::push_json_string(out, s);
}

/// Numbers through serde itself, so ryu/itoa formatting is serde's by
/// construction rather than a reimplementation that could drift.
fn push_wire_i64(out: &mut Vec<u8>, v: i64) {
    serde_json::to_writer(out, &v).expect("writing an i64 into a Vec cannot fail");
}

fn push_wire_f64(out: &mut Vec<u8>, v: f64) {
    // serde emits non-finite floats as null, same as the DOM path
    serde_json::to_writer(out, &v).expect("writing an f64 into a Vec cannot fail");
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
        // upsert built before `text` moves into the inline branch (FK order:
        // msg_part row first, then the projection — push order below)
        let upsert = part_search_upsert_ops(&pid, session_id, &id, &text);
        let compaction_op =
            (ptype == "compaction").then(|| compaction_upsert_ops(&pid, session_id, &id, &text));
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
        ops.push(upsert); // uncompressed text even for blobbed parts (W1)
        if let Some(op) = compaction_op {
            ops.push(op);
        }
    }
    // summary assistants link to their anchor (M6): completedCompactions()
    // reads summary_msg_id from the projection instead of scanning messages
    if info["summary"] == serde_json::Value::Bool(true)
        && let Some(parent) = info["parentID"].as_str()
    {
        ops.push(WriteOp::Sql {
            sql: "UPDATE compaction SET summary_msg_id = ?2 WHERE user_msg_id = ?1 AND summary_msg_id IS NULL".into(),
            params: vec![parent.into(), id.as_str().into()],
        });
    }
    writer.write(ops).map(|_| ())
}

/// Upsert into the content-search projection (W1). Runs the au trigger on
/// conflict → FTS shadow reindexed. Text = the stored part JSON.
///
/// Rowid (PERF-10X S-A, v10): `time_created * 1048576 + slot` where slot =
/// max slot already taken in that ms range + 1 (gaps from deletes never
/// collide; `% 1048576` turns a pathological 1M-parts-in-one-ms overflow
/// into a loud PK conflict instead of a wrong-slot row). msg must exist —
/// the v10 FK + the part_search_rt trigger both enforce it (a NULL rowid
/// would silently auto-assign insert order, i.e. the old broken contract).
/// Parts-per-ms slots in the time-encoded rowid (1_048_576 = 1<<20; with ms
/// epoch ~1.7e12 the rowid fits i64 until year ~2249; max observed parts/ms
/// = 22 — overflow is a PK conflict, never silent).
pub const PART_SEARCH_SLOTS: i64 = 1 << 20;

pub fn part_search_upsert_ops(
    part_id: &str,
    session_id: &str,
    message_id: &str,
    text: &str,
) -> WriteOp {
    let slots = PART_SEARCH_SLOTS;
    WriteOp::Sql {
        sql: format!(
            "INSERT INTO part_search (rowid, part_id, session_id, message_id, text) VALUES ((SELECT (m.time_created * {slots}) + ((SELECT coalesce(max(ps2.rowid) - m.time_created * {slots}, -1) + 1 FROM part_search ps2 WHERE ps2.rowid >= m.time_created * {slots} AND ps2.rowid < (m.time_created + 1) * {slots}) % {slots}) FROM msg m WHERE m.id = ?3), ?1, ?2, ?3, ?4) ON CONFLICT(part_id) DO UPDATE SET text = excluded.text"
        ),
        params: vec![
            part_id.into(),
            session_id.into(),
            message_id.into(),
            text.into(),
        ],
    }
}

/// Compaction projection upsert (M6): one row per anchor part. `auto`/
/// `overflow` come from the part JSON; tail/summary lifecycle fields are
/// updated by separate ops (never clobbered here on conflict).
pub fn compaction_upsert_ops(
    part_id: &str,
    session_id: &str,
    user_msg_id: &str,
    text: &str,
) -> WriteOp {
    let v: serde_json::Value = serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
    let auto = if v["auto"] == serde_json::Value::Bool(true) {
        1
    } else {
        0
    };
    let overflow = if v["overflow"] == serde_json::Value::Bool(true) {
        1
    } else {
        0
    };
    WriteOp::Sql {
        sql: "INSERT INTO compaction (session_id, part_id, user_msg_id, auto, overflow, time_ms) \
              VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
              ON CONFLICT(part_id) DO UPDATE SET auto = excluded.auto, overflow = excluded.overflow"
            .into(),
        params: vec![
            session_id.into(),
            part_id.into(),
            user_msg_id.into(),
            auto.into(),
            overflow.into(),
            now_ms_i64().into(),
        ],
    }
}

/// tail_start_id update when the compaction part gains/changes its tail
/// (upstream: compaction.ts:460-466 updatePart on selected tail change).
pub fn compaction_tail_ops(part_id: &str, data: &serde_json::Value) -> WriteOp {
    let tail = data
        .get("tail_start_id")
        .and_then(|t| t.as_str())
        .map(|t| t.to_string());
    WriteOp::Sql {
        sql: "UPDATE compaction SET tail_start_id = ?2 WHERE part_id = ?1".into(),
        params: vec![part_id.into(), tail.into()],
    }
}

/// Projection rows for one session in creation order (last row = newest
/// anchor — filterCompacted's findLastIndex equivalent, O(compactions)).
pub struct CompactionRow {
    pub part_id: String,
    pub user_msg_id: String,
    pub auto: bool,
    pub overflow: bool,
    pub tail_start_id: Option<String>,
    pub summary_msg_id: Option<String>,
}

pub fn compaction_rows(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> anyhow::Result<Vec<CompactionRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT part_id, user_msg_id, auto, overflow, tail_start_id, summary_msg_id \
         FROM compaction WHERE session_id = ?1 ORDER BY time_ms, rowid",
    )?;
    let rows = stmt
        .query_map([session_id], |r| {
            Ok(CompactionRow {
                part_id: r.get(0)?,
                user_msg_id: r.get(1)?,
                auto: r.get::<_, i64>(2)? != 0,
                overflow: r.get::<_, i64>(3)? != 0,
                tail_start_id: r.get(4)?,
                summary_msg_id: r.get(5)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Prompt-loop preflight (M6 audit fix, COMPACTION §11 hot-path rule):
/// ONE reader answers the round-1 questions — event seq + compaction state —
/// so a normal prompt opens exactly preflight+load_messages (pre-M6 parity).
/// The newest-message query runs ONLY when compaction rows exist (sessions
/// without compaction never pay it; each open_reader would otherwise build a
/// cold page cache + full PRAGMA profile).
pub enum PreflightState {
    /// no pending anchor, last message is not a summary — generate
    Ready,
    /// newest unlinked anchor — run the engine before generation
    Pending {
        anchor: String,
        auto: bool,
        overflow: bool,
    },
    /// last message is a finished summary — manual-summarize exit
    SummaryExit {
        info: serde_json::Value,
        parts: Vec<serde_json::Value>,
    },
}

pub struct Preflight {
    pub seq: i64,
    pub state: PreflightState,
    /// session title (K-TITLE efficiency: retag gate rides this read —
    /// named sessions pay ZERO extra queries per prompt).
    pub title: String,
    /// Session-lifetime usage (the same columns the assistant messages
    /// accumulate into). Seeded once per prompt so `session.updated` events
    /// can carry the truthful row values without re-reading mid-turn.
    pub cost: f64,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_reasoning: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
}

pub fn compaction_preflight(db: &std::path::Path, session_id: &str) -> anyhow::Result<Preflight> {
    let conn = pragma::open_reader(db)?;
    let seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq),0)+1 FROM event WHERE session_id = ?1",
        [session_id],
        |r| r.get(0),
    )?;
    let (title, cost, tin, tout, treason, tcread, tcwrite): (String, f64, i64, i64, i64, i64, i64) =
        conn.query_row(
            "SELECT title, cost, tokens_input, tokens_output, tokens_reasoning, \
             tokens_cache_read, tokens_cache_write FROM session WHERE id = ?1",
            [session_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap_or_default();
    let rows = compaction_rows(&conn, session_id)?;
    if let Some(pending) = rows.iter().rev().find(|r| r.summary_msg_id.is_none()) {
        return Ok(Preflight {
            seq,
            state: PreflightState::Pending {
                anchor: pending.user_msg_id.clone(),
                auto: pending.auto,
                overflow: pending.overflow,
            },
            title,
            cost,
            tokens_input: tin,
            tokens_output: tout,
            tokens_reasoning: treason,
            tokens_cache_read: tcread,
            tokens_cache_write: tcwrite,
        });
    }
    if !rows.is_empty() {
        let blobs_root = db
            .parent()
            .ok_or_else(|| anyhow::anyhow!("db parent"))?
            .join("blobs");
        if let Some((info, parts)) = last_message_conn(&conn, session_id, &blobs_root)?
            && info["role"] == "assistant"
            && info["summary"] == serde_json::Value::Bool(true)
            && info["finish"].is_string()
        {
            return Ok(Preflight {
                seq,
                state: PreflightState::SummaryExit { info, parts },
                title,
                cost,
                tokens_input: tin,
                tokens_output: tout,
                tokens_reasoning: treason,
                tokens_cache_read: tcread,
                tokens_cache_write: tcwrite,
            });
        }
    }
    Ok(Preflight {
        seq,
        state: PreflightState::Ready,
        title,
        cost,
        tokens_input: tin,
        tokens_output: tout,
        tokens_reasoning: treason,
        tokens_cache_read: tcread,
        tokens_cache_write: tcwrite,
    })
}

/// Path-based projection reader (engine call sites — mirrors load_messages).
pub fn compaction_rows_path(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Vec<CompactionRow>> {
    let conn = crate::pragma::open_reader(db)?;
    compaction_rows(&conn, session_id)
}

/// Newest message + parts (summary-exit detection, M6) — O(latest message)
/// via SQL LIMIT 1 + one parts query, NOT a full history load (per-prompt
/// path runs this every round; a full load would double prompt cost).
pub fn last_message(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Option<(serde_json::Value, Vec<serde_json::Value>)>> {
    let conn = pragma::open_reader(db)?;
    let blobs_root = db
        .parent()
        .ok_or_else(|| anyhow::anyhow!("db parent"))?
        .join("blobs");
    last_message_conn(&conn, session_id, &blobs_root)
}

/// Single message WITH parts, or None (v1 `GET /session/{id}/message/{mid}`
/// → {info, parts}; unknown id → 404 NotFoundError envelope).
pub fn message_by_id(
    db: &std::path::Path,
    session_id: &str,
    message_id: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let conn = pragma::open_reader(db)?;
    let blobs_root = db
        .parent()
        .ok_or_else(|| anyhow::anyhow!("db parent"))?
        .join("blobs");
    message_by_id_conn(&conn, session_id, message_id, &blobs_root)
}

pub fn message_by_id_conn(
    conn: &rusqlite::Connection,
    session_id: &str,
    message_id: &str,
    blobs_root: &std::path::Path,
) -> anyhow::Result<Option<serde_json::Value>> {
    let row: Option<String> = conn
        .query_row(
            "SELECT info FROM msg WHERE id = ?1 AND session_id = ?2",
            (message_id, session_id),
            |r| r.get(0),
        )
        .ok();
    let Some(info_txt) = row else {
        return Ok(None);
    };
    let info: serde_json::Value = serde_json::from_str(&info_txt)?;
    let info = merge_columns(info, message_id, session_id, None);
    let parts = load_parts_for_message(conn, message_id, session_id, blobs_root)?;
    Ok(Some(serde_json::json!({"info": info, "parts": parts})))
}

/// Parts for one message (inline read; blob spilled parts decompressed) —
/// same row/merge path as `load_messages` (byte parity by construction).
fn load_parts_for_message(
    conn: &rusqlite::Connection,
    message_id: &str,
    session_id: &str,
    blobs_root: &std::path::Path,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let blobs = crate::blob::BlobStore::new(blobs_root.to_path_buf())?;
    let mut stmt = conn.prepare_cached(
        "SELECT id, inline, blob_sha, byte_len FROM msg_part WHERE message_id = ?1 ORDER BY seq",
    )?;
    let rows: Vec<(String, Option<String>, Option<String>, i64)> = stmt
        .query_map([message_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    let mut out = Vec::with_capacity(rows.len());
    for (part_id, inline, sha, byte_len) in rows {
        let txt = match inline {
            Some(t) => t,
            None => match (&sha, byte_len) {
                (Some(sha), len) => {
                    String::from_utf8_lossy(&blobs.get(sha, len as u64)?).into_owned()
                }
                (None, _) => continue,
            },
        };
        if let Ok(v) = serde_json::from_str(&txt) {
            out.push(merge_columns(v, &part_id, session_id, Some(message_id)));
        }
    }
    Ok(out)
}

/// `last_message` on an EXISTING connection (preflight: zero extra opens).
pub fn last_message_conn(
    conn: &rusqlite::Connection,
    session_id: &str,
    blobs_root: &std::path::Path,
) -> anyhow::Result<Option<(serde_json::Value, Vec<serde_json::Value>)>> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT id, info FROM msg WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
            [session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let Some((mid, info_txt)) = row else {
        return Ok(None);
    };
    let info: serde_json::Value = serde_json::from_str(&info_txt)?;
    let info = merge_columns(info, &mid, session_id, None);
    let blobs = crate::blob::BlobStore::new(blobs_root)?;
    let mut pstmt = conn.prepare_cached(
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
                (None, _) => continue,
            },
        };
        if let Ok(v) = serde_json::from_str(&txt) {
            parts.push(merge_columns(v, &part_id, session_id, Some(&mid)));
        }
    }
    Ok(Some((info, parts)))
}

/// Replace a message's info row (summary finalize: finish/error/completed).
/// Set the session summary counters (upstream `Session.setSummary`).
pub fn set_session_summary(
    writer: &Writer,
    session_id: &str,
    additions: i64,
    deletions: i64,
    files: i64,
) -> anyhow::Result<usize> {
    writer.write(vec![WriteOp::Sql {
        sql: "UPDATE session SET summary_additions = ?2, summary_deletions = ?3, \
              summary_files = ?4 WHERE id = ?1"
            .into(),
        params: vec![
            session_id.into(),
            additions.into(),
            deletions.into(),
            files.into(),
        ],
    }])
}

pub fn update_message_info(
    writer: &Writer,
    session_id: &str,
    message_id: &str,
    info: &serde_json::Value,
) -> anyhow::Result<usize> {
    writer.write(vec![WriteOp::Sql {
        sql: "UPDATE msg SET info = ?3 WHERE id = ?1 AND session_id = ?2".into(),
        params: vec![
            message_id.into(),
            session_id.into(),
            info.to_string().into(),
        ],
    }])
}

fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Boot backfill for pre-existing compaction anchors (M6): legacy imports
/// may carry upstream compaction parts. Idempotent via NOT EXISTS; links
/// summary assistants to their anchors. Returns (rows, links, ms).
pub fn backfill_compaction(
    writer: &Writer,
    db: &std::path::Path,
) -> anyhow::Result<(u64, u64, u64)> {
    let t0 = std::time::Instant::now();
    let conn = crate::pragma::open_reader(db)?;
    // Boot-cost gate (perf dive 2026-10-11): the two scans below run on EVERY
    // boot (msg LIKE + msg_part type scan measured 0.4-10 s on the live DB).
    // They only matter for legacy/pre-v8 imports; the compaction engine
    // maintains the projection directly since v8. A manifest stamp written
    // after the first successful pass turns later boots into one key lookup.
    let stamped: Option<String> = conn
        .query_row(
            "SELECT value FROM import_manifest WHERE key = 'compaction_backfilled_v1'",
            [],
            |r| r.get(0),
        )
        .ok();
    if stamped.as_deref() == Some("1") {
        drop(conn);
        return Ok((0, 0, t0.elapsed().as_millis() as u64));
    }
    let mut rows: Vec<(String, String, String, String, i64, i64)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT p.id, p.message_id, p.session_id, COALESCE(p.inline, ''), m.info \
             FROM msg_part p JOIN msg m ON m.id = p.message_id \
             WHERE p.type = 'compaction' \
               AND NOT EXISTS (SELECT 1 FROM compaction c WHERE c.part_id = p.id)",
        )?;
        let mapped = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for row in mapped.flatten() {
            let (part_id, msg_id, session_id, inline, info_json) = row;
            let info: serde_json::Value = serde_json::from_str(&info_json).unwrap_or_default();
            let _ = info;
            rows.push((part_id, msg_id, session_id, inline, 0, 0));
        }
    }
    let mut n = 0u64;
    // Batched (perf dive 2026-10-11): the pre-fix loop committed one
    // transaction per anchor (~5 s class on thousands of anchors).
    let mut ops: Vec<WriteOp> = Vec::with_capacity(256);
    for (part_id, msg_id, session_id, inline, _, _) in &rows {
        if inline.is_empty() {
            continue; // blobbed anchor (never in practice) — counted below
        }
        ops.push(compaction_upsert_ops(part_id, session_id, msg_id, inline));
        n += 1;
        if ops.len() >= 256 {
            writer.write(std::mem::take(&mut ops))?;
        }
    }
    if !ops.is_empty() {
        writer.write(ops)?;
    }
    // link summary assistants (info has summary:true + parentID)
    let mut links = 0u64;
    let mut link_ops: Vec<WriteOp> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, info FROM msg WHERE role = 'assistant' \
             AND info LIKE '%\"summary\":true%' AND info LIKE '%parentID%'",
        )?;
        let ids: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        for (sid, info_json) in ids {
            let info: serde_json::Value = match serde_json::from_str(&info_json) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(parent) = info["parentID"].as_str() else {
                continue;
            };
            // only count ACTUAL links (a second run must report zero)
            let pending: i64 = conn.query_row(
                "SELECT count(*) FROM compaction WHERE user_msg_id = ?1 AND summary_msg_id IS NULL",
                [parent],
                |r| r.get(0),
            )?;
            if pending == 0 {
                continue;
            }
            link_ops.push(WriteOp::Sql {
                sql: "UPDATE compaction SET summary_msg_id = ?2 WHERE user_msg_id = ?1 AND summary_msg_id IS NULL".into(),
                params: vec![parent.into(), sid.as_str().into()],
            });
            links += 1;
        }
    }
    if !link_ops.is_empty() {
        writer.write(link_ops)?;
    }
    // stamp so future boots skip the two scans (see the gate above).
    // PROPAGATE (AGENTS §2.5): a write failure on a healthy writer means
    // storage is broken — swallowing it left the boot re-scanning forever and
    // hid a real error (guard rule 1 caught this on 2026-10-12).
    writer.write(vec![WriteOp::Sql {
        sql: "INSERT OR REPLACE INTO import_manifest (key, value) VALUES ('compaction_backfilled_v1', '1')".into(),
        params: vec![],
    }])?;
    Ok((n, links, t0.elapsed().as_millis() as u64))
}

/// One part row's write pair (msg_part INSERT with an EXPLICIT seq +
/// part_search upsert). The only legal way for code OUTSIDE ocserve-store
/// (importer, sync) to create parts — check-guards.sh rule 4 enforces it.
pub struct PartRow<'a> {
    pub id: &'a str,
    pub message_id: &'a str,
    pub session_id: &'a str,
    pub seq: i64,
    pub ptype: &'a str,
    pub byte_len: i64,
    pub inline: Option<String>,
    pub blob_sha: Option<String>,
    pub text: &'a str,
}

pub fn part_row_ops(row: &PartRow<'_>) -> Vec<WriteOp> {
    let mut ops = vec![
        WriteOp::Sql {
            sql: "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) \
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
                .into(),
            params: vec![
                row.id.into(),
                row.message_id.into(),
                row.session_id.into(),
                row.seq.into(),
                row.ptype.into(),
                row.byte_len.into(),
                row.inline.clone().into(),
                row.blob_sha.clone().into(),
            ],
        },
        part_search_upsert_ops(row.id, row.session_id, row.message_id, row.text),
    ];
    if row.ptype == "compaction" {
        ops.push(compaction_upsert_ops(
            row.id,
            row.session_id,
            row.message_id,
            row.text,
        ));
    }
    ops
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
            let mut stmt = conn.prepare_cached(
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
            let mut stmt = conn.prepare_cached(
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

/// Project one page of messages to v2 `Session.Message` shapes with REAL
/// `content[]` (V2-PLAN.md P4 — replaces the skeletal `"content": []`).
///
/// Reads parts for the page's messages in ONE `IN` query per 64-message chunk
/// (same bound as F3 — never a per-message query, never a whole-session
/// prefetch). Part-type mapping:
/// - `text` → `{type:"text", text, state:"completed"}`
/// - `reasoning` → `{type:"reasoning", text, state:"completed"}`
/// - `tool` → `{type:"tool", id, name, state, time}` from the stored state
///   envelope (`{status,input,output,error,time}` → v2
///   `completed{input,output}|error{input,error}|running{input}`)
/// - `step-start`/`step-finish`/`patch`/`idle`/`busy`/`function` →
///   folded into the assistant envelope (`snapshot`, `cost`, `finish`), never
///   content elements (upstream has no message.part.* in v2 either)
/// - user messages → `{type:"user", text, files, agents}` from info + file
///   parts (upstream `Session.Message.User`)
///
/// Blobbed parts decode through the same `BlobStore` path as production.
/// Explicit columns everywhere (rule 25/33).
pub fn project_page_v2(
    db: &std::path::Path,
    page: &[(String, String)],
) -> anyhow::Result<Vec<serde_json::Value>> {
    use serde_json::{Value, json};
    let conn = pragma::open_reader(db)?;
    let blobs = crate::blob::BlobStore::new(
        db.parent()
            .ok_or_else(|| anyhow::anyhow!("db parent"))?
            .join("blobs"),
    )?;
    // Batch parts per 64-chunk (F3 bound).
    const CHUNK: usize = 64;
    let placeholders = vec!["?"; CHUNK].join(",");
    let mut pstmt = conn.prepare_cached(&format!(
        "SELECT message_id, type, inline, blob_sha, byte_len FROM msg_part \
         WHERE message_id IN ({placeholders}) ORDER BY message_id, seq"
    ))?;
    type PartRow = (String, Option<String>, Option<(String, i64)>);
    let mut parts: std::collections::HashMap<String, Vec<PartRow>> =
        std::collections::HashMap::new();
    let mut padded: Vec<Option<String>> = Vec::with_capacity(CHUNK);
    let mut iter = page.iter();
    loop {
        let head: Vec<&(String, String)> = iter.by_ref().take(CHUNK).collect();
        if head.is_empty() {
            break;
        }
        padded.clear();
        for (mid, _) in &head {
            padded.push(Some((*mid).clone()));
        }
        while padded.len() < CHUNK {
            padded.push(None);
        }
        let mut rows = pstmt.query(rusqlite::params_from_iter(padded.iter()))?;
        while let Some(row) = rows.next()? {
            let sha: Option<String> = row.get(3)?;
            let len: i64 = row.get(4)?;
            parts.entry(row.get::<_, String>(0)?).or_default().push((
                row.get(1)?,
                row.get(2)?,
                sha.map(|s| (s, len)),
            ));
        }
    }
    let empty = Vec::new();
    let mut out = Vec::with_capacity(page.len());
    for (mid, info_txt) in page {
        let info: Value = serde_json::from_str(info_txt).unwrap_or(Value::Null);
        let role = info.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let created = info.pointer("/time/created").cloned().unwrap_or(json!(0));
        let time = json!({"created": created});
        let plist = parts.get(mid).map(|v| v.as_slice()).unwrap_or(&empty);
        let mut content = Vec::new();
        for (ptype, inline, blob) in plist {
            let text_of = || -> String {
                if let Some(t) = inline {
                    return t.clone();
                }
                if let Some((s, len)) = blob
                    && let Ok(bytes) = blobs.get(s, *len as u64)
                {
                    return String::from_utf8_lossy(&bytes).into_owned();
                }
                String::new()
            };
            match ptype.as_str() {
                "text" => {
                    let raw = text_of();
                    let body: Value = serde_json::from_str(&raw).unwrap_or(json!({"text": ""}));
                    content.push(json!({
                        "type": "text",
                        "text": body.get("text").cloned().unwrap_or(json!("")),
                        "state": "completed",
                    }));
                }
                "reasoning" => {
                    let raw = text_of();
                    let body: Value = serde_json::from_str(&raw).unwrap_or(json!({"text": ""}));
                    content.push(json!({
                        "type": "reasoning",
                        "text": body.get("text").cloned().unwrap_or(json!("")),
                        "state": "completed",
                    }));
                }
                "tool" => {
                    let raw = text_of();
                    let body: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
                    let state = body.get("state").cloned().unwrap_or(json!({}));
                    let status = state
                        .get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("running");
                    let input = state.get("input").cloned().unwrap_or(json!({}));
                    let v2state = match status {
                        "completed" => json!({
                            "status": "completed",
                            "input": input,
                            "output": state.get("output").cloned().unwrap_or(json!([])),
                        }),
                        "error" => json!({
                            "status": "error",
                            "input": input,
                            "error": state.get("error").cloned().unwrap_or(json!("")),
                        }),
                        _ => json!({"status": "running", "input": input}),
                    };
                    content.push(json!({
                        "type": "tool",
                        "id": body.get("callID").cloned().unwrap_or(json!("")),
                        "name": body.get("tool").cloned().unwrap_or(json!("")),
                        "state": v2state,
                        "time": state.get("time").cloned().unwrap_or(json!({})),
                    }));
                }
                _ => {} // step-start/finish, patch, idle, busy, compaction: envelope, not content
            }
        }
        match role {
            "user" => {
                let text = info
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                // file/agent attachments travel as parts; surface the file ones.
                let mut files = Vec::new();
                let mut agents = Vec::new();
                for (ptype, inline, _blob) in plist {
                    if ptype == "file"
                        && let Some(t) = inline
                        && let Ok(v) = serde_json::from_str::<Value>(t)
                    {
                        files.push(v);
                    } else if ptype == "agent"
                        && let Some(t) = inline
                        && let Ok(v) = serde_json::from_str::<Value>(t)
                    {
                        agents.push(v);
                    }
                }
                out.push(json!({
                    "id": mid, "time": time, "type": "user",
                    "text": text, "files": files, "agents": agents,
                }));
            }
            "assistant" => {
                out.push(json!({
                    "id": mid, "time": time, "type": "assistant",
                    "agent": info.get("agent").cloned().unwrap_or(json!("build")),
                    "model": {
                        "id": info.get("modelID").cloned().unwrap_or(json!("")),
                        "providerID": info.get("providerID").cloned().unwrap_or(json!("")),
                    },
                    "content": content,
                    "cost": info.get("cost").cloned().unwrap_or(json!(0)),
                    "tokens": info.get("tokens").cloned().unwrap_or(json!({})),
                }));
            }
            _ => {}
        }
    }
    Ok(out)
}
/// Walk a session's messages one at a time, yielding each message's FULL
/// response JSON object (`{"info":…,"parts":[…]}`) to `visit`. Bounded by
/// design (AGENTS §2.3): peak = one message — the old Vec<Value> path
/// OOM-killed the cgroup on a 16k-message session (93MB response ≈ 400MB+
/// as serde Values). Column merge applied per row (see merge_columns).
/// What to walk: full/tail history (seq order) or an explicit pre-ordered
/// cursor window from `page_messages` (tuple order).
#[derive(Clone)]
pub enum MessageWalk {
    /// last `limit` messages in seq order (`None` = all)
    Seq { limit: Option<usize> },
    /// pre-computed (id, info) window in response order (already ASC)
    Window(Vec<(String, String)>),
}

pub mod splice;

/// L1 splice fallbacks (DOM path taken) since process start. The corpus
/// differential proves 0 on real data; anything non-zero means stored bytes
/// hit a shape the splicer will not guess at, which is a signal, not a
/// silent fallback. Exported for the metric + tests.
pub static SPLICE_FALLBACKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn splice_fallbacks() -> u64 {
    SPLICE_FALLBACKS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Rows that took the zero-parse path since process start.
pub fn splice_rows() -> u64 {
    SPLICE_ROWS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Splice throughput counters: (rows spliced, rows sent to the DOM path).
static SPLICE_ROWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
            // legacy/ocserve timelines stay chronological (seq = write-order
            // bookkeeping only; sync appends older-by-time rows later)
            let mut stmt = match limit {
                Some(_) => conn.prepare_cached(
                    "SELECT id, info FROM (SELECT id, info, time_created FROM msg WHERE session_id = ?1 \
                     ORDER BY time_created DESC, id DESC LIMIT ?2) ORDER BY time_created, id",
                )?,
                None => conn.prepare_cached(
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
    // F3 (PERF-10X): parts prefetched in chunks of 64 messages — one IN
    // query per chunk instead of one per message (51 queries -> 1 per
    // page; measured 11ms on the deep session, becomes the per-request
    // floor once S-A removed the tail). Chunking (not one big query) is
    // load-bearing: a whole-session prefetch of the 16k-message stream
    // would hold every inline part in RAM at once — the exact OOM class
    // the bounded channel exists for (AGENTS §2.3). Peak = 64 messages'
    // parts. Statement prepared ONCE (fixed 64 params; trailing NULLs
    // never match a real id).
    const PART_CHUNK: usize = 64;
    let placeholders = vec!["?"; PART_CHUNK].join(",");
    let mut pstmt = conn.prepare_cached(&format!(
        "SELECT message_id, id, inline, blob_sha, byte_len FROM msg_part WHERE message_id IN ({placeholders}) ORDER BY message_id, seq"
    ))?;
    #[derive(Debug)]
    struct PartRow {
        id: String,
        inline: Option<String>,
        sha: Option<String>,
        byte_len: i64,
    }
    // K-EFFICIENCY (bytehound group #2): ONE reusable serializer buffer per
    // fetch — Value::to_string grew a fresh String per message/part (top
    // churn group in the profile). The frame itself is still an owned
    // String (it crosses the channel); everything nested is append-only.
    let mut ser: Vec<u8> = Vec::with_capacity(64 * 1024);
    // L1 splice scratch (key set + one decoded-key slot), reused per row
    let mut scratch = crate::splice::SpliceScratch::default();
    let mut groups: std::collections::HashMap<String, Vec<PartRow>> =
        std::collections::HashMap::new();
    let mut padded: Vec<Option<String>> = Vec::with_capacity(PART_CHUNK);
    let mut msg_iter = msgs.into_iter();
    loop {
        // next up-to-64 messages: prefetch their parts in ONE query, then
        // emit frames in message order (groups drained per mid).
        let head: Vec<(String, String)> = (&mut msg_iter).take(PART_CHUNK).collect();
        if head.is_empty() {
            break;
        }
        groups.clear();
        padded.clear();
        for (mid, _) in &head {
            padded.push(Some(mid.clone()));
        }
        while padded.len() < PART_CHUNK {
            padded.push(None);
        }
        {
            let mut rows = pstmt.query(rusqlite::params_from_iter(padded.iter()))?;
            while let Some(row) = rows.next()? {
                groups
                    .entry(row.get::<_, String>(0)?)
                    .or_default()
                    .push(PartRow {
                        id: row.get(1)?,
                        inline: row.get(2)?,
                        sha: row.get(3)?,
                        byte_len: row.get(4)?,
                    });
            }
        }
        for (mid, info_txt) in &head {
            // L1 (PERF-10X Phase II): zero-parse assembly. Phase I attributed
            // 86% of read-path allocations to serde_json DOM work, so the
            // splice replaces parse→merge→serialize with one byte pass into a
            // REUSED buffer. Proven byte-identical to the DOM path over the
            // whole stored corpus (218,393 rows, 0 refused, 0 mismatched —
            // `tests/splice_parity.rs::splice_parity_over_corpus`), and it
            // falls back to the DOM path on anything it cannot prove, so the
            // fast path can never change semantics.
            let mut chunk = String::with_capacity(info_txt.len() + 8192);
            chunk.push_str("{\"info\":");
            ser.clear();
            let spliced_info = crate::splice::compact_splice(
                info_txt,
                mid,
                session_id,
                None,
                &mut ser,
                &mut scratch,
            );
            if spliced_info {
                SPLICE_ROWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                chunk.push_str(std::str::from_utf8(&ser)?);
            } else {
                // DOM fallback — unreachable on real data, proven by the
                // corpus differential, and never silent
                let info: serde_json::Value = serde_json::from_str(info_txt)?;
                let info = merge_columns(info, mid, session_id, None);
                ser.clear();
                serde_json::to_writer(&mut ser, &info)?;
                chunk.push_str(std::str::from_utf8(&ser)?);
                SPLICE_FALLBACKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            chunk.push_str(",\"parts\":[");
            let parts = groups.remove(mid).unwrap_or_default();
            let mut first_part = true;
            for prow in &parts {
                if !first_part {
                    chunk.push(',');
                }
                first_part = false;
                // blob parts: decode straight from bytes (no lossy String copy)
                let inline: Option<std::borrow::Cow<'_, str>> = match &prow.inline {
                    Some(t) => Some(std::borrow::Cow::Borrowed(t.as_str())),
                    None => match (&prow.sha, prow.byte_len) {
                        (Some(sha), len) => match blobs.get(sha, len as u64) {
                            Ok(raw) => match String::from_utf8(raw) {
                                Ok(s) => Some(std::borrow::Cow::Owned(s)),
                                Err(e) => Some(std::borrow::Cow::Owned(
                                    String::from_utf8_lossy(e.as_bytes()).into_owned(),
                                )),
                            },
                            Err(e) => {
                                // unreadable blob: skip the part exactly as the
                                // DOM path did (it errored on `?`) — but now
                                // loud, because silently dropping a part would
                                // be a wire change
                                tracing::warn!("page part blob {sha} unreadable: {e:#}");
                                SPLICE_FALLBACKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                chunk.pop(); // undo the separator we just wrote
                                first_part = true;
                                continue;
                            }
                        },
                        (None, _) => {
                            chunk.pop();
                            first_part = true;
                            continue;
                        }
                    },
                };
                let Some(text) = inline else { continue };
                ser.clear();
                if crate::splice::compact_splice(
                    &text,
                    &prow.id,
                    session_id,
                    Some(mid),
                    &mut ser,
                    &mut scratch,
                ) {
                    SPLICE_ROWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    chunk.push_str(std::str::from_utf8(&ser)?);
                } else {
                    let v: serde_json::Value = serde_json::from_str(&text)?;
                    let merged = merge_columns(v, &prow.id, session_id, Some(mid));
                    ser.clear();
                    serde_json::to_writer(&mut ser, &merged)?;
                    chunk.push_str(std::str::from_utf8(&ser)?);
                    SPLICE_FALLBACKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            chunk.push_str("]}");
            visit(chunk)?;
        }
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
        Some(_) => conn.prepare_cached(
            "SELECT id, info FROM (SELECT id, info, seq FROM msg WHERE session_id = ?1 \
             ORDER BY seq DESC LIMIT ?2) ORDER BY seq",
        )?,
        None => {
            conn.prepare_cached("SELECT id, info FROM msg WHERE session_id = ?1 ORDER BY seq")?
        }
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
    // Chunked part prefetch (perf dive 2026-10-11): the pre-fix loop ran one
    // prepared query PER MESSAGE (N+1; 16.6k messages on the deep session).
    // Same 64-chunk IN-prefetch the streaming `for_each_message_json` path
    // already uses (F3). Groups keyed by message_id; emission stays in
    // message order.
    const PART_CHUNK: usize = 64;
    let placeholders = vec!["?"; PART_CHUNK].join(",");
    let mut pstmt = conn.prepare_cached(&format!(
        "SELECT message_id, id, inline, blob_sha, byte_len FROM msg_part \
         WHERE message_id IN ({placeholders}) ORDER BY message_id, seq"
    ))?;
    let mut padded: Vec<Option<String>> = Vec::with_capacity(PART_CHUNK);
    /// (part_id, inline, blob_sha, byte_len) as stored.
    type PartLite = (String, Option<String>, Option<String>, i64);
    let mut groups: std::collections::HashMap<String, Vec<PartLite>> =
        std::collections::HashMap::new();
    let mut msg_iter = msgs.into_iter().peekable();
    while msg_iter.peek().is_some() {
        let head: Vec<(String, String)> = (&mut msg_iter).take(PART_CHUNK).collect();
        groups.clear();
        padded.clear();
        for (mid, _) in &head {
            padded.push(Some(mid.clone()));
        }
        while padded.len() < PART_CHUNK {
            padded.push(None);
        }
        {
            let mut rows = pstmt.query(rusqlite::params_from_iter(padded.iter()))?;
            while let Some(row) = rows.next()? {
                groups.entry(row.get::<_, String>(0)?).or_default().push((
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ));
            }
        }
        for (mid, info_txt) in head {
            let info: serde_json::Value = serde_json::from_str(&info_txt)?;
            let info = merge_columns(info, &mid, session_id, None);
            let mut parts: Vec<serde_json::Value> = Vec::new();
            if let Some(rows) = groups.get(&mid) {
                for (part_id, inline, sha, byte_len) in rows {
                    let txt = match inline {
                        Some(t) => t.clone(),
                        None => match (sha, byte_len) {
                            (Some(sha), len) => {
                                String::from_utf8_lossy(&blobs.get(sha, *len as u64)?).into_owned()
                            }
                            (None, _) => continue, // corrupt row: no payload at all
                        },
                    };
                    if let Ok(v) = serde_json::from_str(&txt) {
                        parts.push(merge_columns(v, part_id, session_id, Some(&mid)));
                    }
                }
            }
            out.push((info, parts));
        }
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

/// v2 inbox engine (V2-PLAN.md P3). Server-owned pending-work queue mirroring
/// upstream's admission → promote → deliver flow (`inbox.ts`):
/// - admit: idempotent first-write-wins on `(session_id, type, id)`
///   (cross-session/type reuse is a caller-level 409 — `reconcile` reports it)
/// - deliver: delete the inbox row + insert the visible message in ONE op
///   batch (atomic on the writer)
/// - promote order: steers first, then exactly one queued item; compaction
///   jumps ahead of steers but never across a move (caller enforces)
/// - cancel/update: guarded (delivered/missing rows report `Gone`, not Ok)
///
/// All SQL names explicit columns (rule 25/33). Payloads are caller JSON.
pub mod inbox {
    use super::{WriteOp, Writer};
    use anyhow::Result;

    /// One inbox row: (id, item_type, payload_json, delivery, enqueued_seq).
    pub type Item = (String, String, String, String, i64);

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// Admit an inbox item. Returns `true` when newly inserted, `false` when
    /// the id already exists (idempotent reconcile — first write wins).
    pub fn admit(
        writer: &Writer,
        session_id: &str,
        id: &str,
        item_type: &str,
        payload: &serde_json::Value,
        delivery: &str,
        enqueued_seq: i64,
    ) -> Result<bool> {
        let n = writer.write(vec![WriteOp::Sql {
            sql: "INSERT OR IGNORE INTO session_inbox \
                  (id, session_id, type, payload, delivery, enqueued_seq, time_created) \
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                .into(),
            params: vec![
                id.into(),
                session_id.into(),
                item_type.into(),
                payload.to_string().into(),
                delivery.into(),
                enqueued_seq.into(),
                now_ms().into(),
            ],
        }])?;
        Ok(n == 1)
    }

    /// Fetch one pending item by id (explicit columns, rule 33).
    pub fn get(db: &std::path::Path, id: &str) -> Result<Option<Item>> {
        let conn = crate::pragma::open_reader(db)?;
        let mut stmt = conn.prepare_cached(
            "SELECT session_id, type, payload, delivery, enqueued_seq \
             FROM session_inbox WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        Ok(rows.next().transpose()?)
    }

    /// List pending items for a session in promote order
    /// (enqueued_seq; caller partitions steer/queue/compaction/move).
    /// Cheap "is there queued work for this session" probe (the post-turn wake
    /// gate). A count rather than a list so a 10k backlog does not allocate.
    pub fn pending_count(db: &std::path::Path, session_id: &str) -> Result<i64> {
        let conn = crate::pragma::open_reader(db)?;
        conn.query_row(
            "SELECT count(*) FROM session_inbox WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    pub fn list(db: &std::path::Path, session_id: &str) -> Result<Vec<Item>> {
        let conn = crate::pragma::open_reader(db)?;
        let mut stmt = conn.prepare_cached(
            "SELECT id, type, payload, delivery, enqueued_seq FROM session_inbox \
             WHERE session_id = ?1 ORDER BY enqueued_seq ASC",
        )?;
        let rows = stmt.query_map([session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Cancel a pending item. Returns `true` when a row was deleted,
    /// `false` when already delivered/missing (caller maps to no-op/409).
    pub fn cancel(writer: &Writer, id: &str) -> Result<bool> {
        let n = writer.write(vec![WriteOp::Sql {
            sql: "DELETE FROM session_inbox WHERE id = ?1".into(),
            params: vec![id.into()],
        }])?;
        Ok(n == 1)
    }

    /// Flip a pending item's delivery. Returns `true` on update.
    pub fn set_delivery(writer: &Writer, id: &str, delivery: &str) -> Result<bool> {
        let n = writer.write(vec![WriteOp::Sql {
            sql: "UPDATE session_inbox SET delivery = ?2 WHERE id = ?1".into(),
            params: vec![id.into(), delivery.into()],
        }])?;
        Ok(n == 1)
    }

    /// Deliver: delete the inbox row. The caller appends the visible-message
    /// insert ops to the SAME writer batch for atomicity — this helper builds
    /// the delete op; see `deliver_ops`.
    pub fn deliver_ops(id: &str) -> WriteOp {
        WriteOp::Sql {
            sql: "DELETE FROM session_inbox WHERE id = ?1".into(),
            params: vec![id.to_string().into()],
        }
    }

    /// Next promotable item id for a session: lowest-seq row, with the
    /// caller's scope filter applied by choosing `deliveries` (e.g.
    /// `("steer")` at a step boundary, `("steer","queue")` at idle).
    /// Compaction-ahead-of-steers is a caller-side pick over `list()`.
    pub fn next_promotable(
        db: &std::path::Path,
        session_id: &str,
        deliveries: &[&str],
    ) -> Result<Option<String>> {
        let conn = crate::pragma::open_reader(db)?;
        let placeholders = deliveries.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id FROM session_inbox WHERE session_id = ?1 \
             AND delivery IN ({placeholders}) ORDER BY enqueued_seq ASC LIMIT 1"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&session_id];
        for d in deliveries {
            params.push(d);
        }
        let mut rows = stmt.query_map(params.as_slice(), |r| r.get::<_, String>(0))?;
        Ok(rows.next().transpose()?)
    }
}

/// Execution claims (V2-PLAN.md P3).
///
/// Claims are restart-recovery markers only (NOT fencing, NOT exactly-once):
/// `claim` inserts iff no claim is held (re-claim no-op); `release` clears;
/// `count_resume` increments the attempt budget (terminal at 10 — upstream
/// `maxAttempts`). Boot recovery lists suspended sessions.
///
/// Claims live in their OWN table (`execution_claim`), not in
/// `instruction_entry`: the journal is instruction domain state and a claim
/// marker there would pollute epoch snapshots and entry listings.
pub mod claim {
    use super::{WriteOp, Writer};
    use anyhow::Result;

    /// Maximum resume attempts before a suspended session terminal-fails.
    pub const MAX_RESUME_ATTEMPTS: i64 = 10;

    /// Claim a session for execution. Returns `true` when newly claimed,
    /// `false` when already held (re-claim no-op).
    pub fn claim(writer: &Writer, session_id: &str, now_ms: i64) -> Result<bool> {
        let n = writer.write(vec![WriteOp::Sql {
            sql: "INSERT OR IGNORE INTO execution_claim (session_id, claimed_ms, resume_attempts) \
                  VALUES (?1, ?2, 0)"
                .into(),
            params: vec![session_id.into(), now_ms.into()],
        }])?;
        Ok(n == 1)
    }

    /// Release a claim. Returns `true` when a claim was held.
    pub fn release(writer: &Writer, session_id: &str) -> Result<bool> {
        let n = writer.write(vec![WriteOp::Sql {
            sql: "DELETE FROM execution_claim WHERE session_id = ?1".into(),
            params: vec![session_id.into()],
        }])?;
        Ok(n == 1)
    }

    /// True when the session currently holds a claim.
    pub fn held(db: &std::path::Path, session_id: &str) -> Result<bool> {
        let conn = crate::pragma::open_reader(db)?;
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM execution_claim WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Increment the resume-attempt budget; returns the new total.
    pub fn count_resume(writer: &Writer, session_id: &str, now_ms: i64) -> Result<i64> {
        writer.write(vec![WriteOp::Sql {
            sql: "UPDATE execution_claim SET resume_attempts = resume_attempts + 1, \
                  claimed_ms = ?2 WHERE session_id = ?1"
                .into(),
            params: vec![session_id.into(), now_ms.into()],
        }])?;
        let conn = crate::pragma::open_reader(writer.db_path_ref())?;
        conn.query_row(
            "SELECT resume_attempts FROM execution_claim WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    /// Sessions holding claims (boot recovery scan). Bounded: at most the
    /// sessions with live claims — claims release on terminal states.
    pub fn list_suspended(db: &std::path::Path) -> Result<Vec<String>> {
        let conn = crate::pragma::open_reader(db)?;
        let mut stmt = conn.prepare_cached("SELECT session_id FROM execution_claim")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}
pub const EVENT_MAX_PER_SESSION: i64 = 200_000;
pub const EVENT_MAX_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Versioned event type discipline (V2-PLAN.md P2, C1 amendment).
///
/// Upstream v2 versions durable event types (`type@vN`) and addresses events
/// by stable `evt_` ids with a per-aggregate `{aggregateID, seq, version}`
/// envelope. Our `event` table carries the unversioned v1 type and an
/// AUTOINCREMENT seq, and the two emitters (`prompt.rs emit_durable`,
/// `lib.rs emit_durable_http`) each mint ids differently. This function is the
/// SINGLE choke through which every durable emission flows (rule: no direct
/// `append_event` calls from server paths — the guard enforces it):
/// it stamps the versioned type, mints the stable id from the allocated seq,
/// appends the row, and returns `(seq, id)` so the live frames and the stored
/// row agree byte-for-byte. One write, two grammars (D5).
pub fn emit_event(
    writer: &Writer,
    session_id: Option<&str>,
    event_type: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<(i64, String)> {
    let versioned = versioned_event_type(event_type);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    // Allocate the seq INSIDE the write transaction via `RETURNING seq`.
    //
    // The previous implementation appended, then opened a SEPARATE reader for
    // `SELECT MAX(seq) WHERE session_id`. Its comment claimed the writer
    // serialized all appends; that is true of the INSERT and false of the
    // window after the commit. Two emitters on one session could both read the
    // same MAX: `tests/event_seq.rs` measured 115 duplicate seq values across
    // 200 concurrent emits before this change. Upstream allocates inside the
    // transaction (`core/src/event.ts:318`, `EventSequenceTable` with
    // `onConflictDoUpdate`), so the read-back was a mechanism divergence, not
    // a shared quirk.
    //
    // The durable seq feeds the v1 sync-twin `seq` and the inbox
    // `enqueued_seq` (ordered by `inbox_list`). The v2 log watermark uses the
    // table's AUTOINCREMENT rowid and was never affected.
    let ack = writer.write_capture(vec![WriteOp::SqlReturning {
        sql: "INSERT INTO event (session_id, project_id, type, payload, time_created) \
              VALUES (?1, 'global', ?2, ?3, ?4) RETURNING seq"
            .into(),
        params: vec![
            session_id.map(|s| s.to_string()).into(),
            versioned.into(),
            payload.to_string().into(),
            now.into(),
        ],
        capture: crate::writer::Capture::FirstColumnI64,
    }])?;
    let seq = ack
        .captured
        .first()
        .copied()
        .ok_or_else(|| anyhow::anyhow!("event insert returned no seq"))?;
    // Retention stays on the same throttled tick as before.
    crate::maybe_prune_events(writer, now)?;
    Ok((seq, format!("evt_{seq}")))
}

/// Versioned append WITHOUT the seq read-back (hot prompt path — V2-PLAN.md
/// P2/C4: the per-emission MAX query showed up as pure overhead once the
/// prompt-local counter proved authoritative for sync twins; admission paths
/// that need the durable seq keep using `emit_event`).
pub fn append_versioned(
    writer: &Writer,
    session_id: Option<&str>,
    event_type: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<()> {
    append_event(
        writer,
        session_id,
        &versioned_event_type(event_type),
        payload,
    )
}

/// Suffix the durable version onto an event type (`type` → `type@v1`).
/// Already-versioned types pass through; the v1 sync-twin suffix (`.1`) is
/// preserved (it addresses the sync envelope, not the durable version).
pub fn versioned_event_type(event_type: &str) -> String {
    if event_type.contains('@') {
        return event_type.to_string();
    }
    format!("{event_type}@v1")
}

/// Strip the durable version suffix (`type@vN` → `type`). The live v1 frames
/// and the event-log validator operate on unversioned types.
pub fn unversioned_event_type(event_type: &str) -> &str {
    match event_type.rsplit_once('@') {
        Some((base, suffix))
            if suffix.len() > 1
                && suffix.starts_with('v')
                && suffix[1..].chars().all(|c| c.is_ascii_digit()) =>
        {
            base
        }
        _ => event_type,
    }
}

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
    // Age prune: idx_event_time seek (v12) — was an unindexed full scan.
    conn.execute(
        "DELETE FROM event WHERE time_created < ?1 AND time_created <> 0",
        [cutoff],
    )?;
    let aged = conn.changes() as usize;
    // Cap window (perf dive 2026-10-11): the ROW_NUMBER window materializes
    // every row into a temp b-tree on EVERY tick even though no session is
    // near the cap. Gate it behind a covering-idx_event_session existence
    // check (early-exits when a session IS over; index-only scan otherwise).
    let over: i64 = conn.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM event GROUP BY session_id HAVING count(*) > ?1 LIMIT 1)",
        [max_per_session],
        |r| r.get(0),
    )?;
    let mut capped = 0;
    if over > 0 {
        capped = conn.execute(
            "DELETE FROM event WHERE rowid IN (
                SELECT rowid FROM (
                    SELECT rowid, ROW_NUMBER() OVER (
                        PARTITION BY session_id ORDER BY seq DESC
                    ) AS rn FROM event
                ) WHERE rn > ?1
            )",
            [max_per_session],
        )?;
    }
    Ok(aged + capped)
}

/// Full `SessionStats.Info` projection (V2-PARITY Phase F: the validator
/// proved the summary shape insufficient). Every figure is a columnar
/// aggregate — indexed COUNT/SUM/GROUP BY, never per-row JSON shredding:
/// - cost: SUM(session.cost) (lifetime accumulator, token_accounting.rs)
/// - tools: summary mode with real status split from
///   `msg_part.inline.state.status` (completed/error/other→unfinished)
///   grouped by tool name (bounded: distinct tools, not rows)
/// - activeDays/streak/activity: GROUP BY day(time_created) over msg
///   (bounded: distinct days); streak = trailing consecutive days to today
/// - models: GROUP BY (model) with step/token/cost SUMs (bounded: models)
pub fn session_stats_full(db: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let conn = pragma::open_reader(db)?;
    #[allow(clippy::type_complexity)]
    let (sessions, prompts, steps, tin, tout, treason, tcr, tcw, tcost, tmin, tmax): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        f64,
        Option<i64>,
        Option<i64>,
    ) = conn.query_row(
        "SELECT (SELECT count(*) FROM session), \
                (SELECT count(*) FROM msg WHERE role = 'user'), \
                (SELECT count(*) FROM msg WHERE role = 'assistant'), \
                (SELECT COALESCE(SUM(tokens_input),0) FROM session), \
                (SELECT COALESCE(SUM(tokens_output),0) FROM session), \
                (SELECT COALESCE(SUM(tokens_reasoning),0) FROM session), \
                (SELECT COALESCE(SUM(tokens_cache_read),0) FROM session), \
                (SELECT COALESCE(SUM(tokens_cache_write),0) FROM session), \
                (SELECT COALESCE(SUM(cost),0.0) FROM session), \
                (SELECT MIN(time_created) FROM session), \
                (SELECT MAX(time_updated) FROM session)",
        [],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
            ))
        },
    )?;
    // Tool status split: completed → succeeded, error → failed, anything
    // else (running/absent) → unfinished. One GROUP BY, not per-row decode.
    let mut stmt = conn.prepare_cached(
        "SELECT json_extract(inline, '$.tool'), json_extract(inline, '$.state.status'), count(*) \
         FROM msg_part WHERE type = 'tool' GROUP BY 1, 2",
    )?;
    let mut calls: i64 = 0;
    let mut succeeded: i64 = 0;
    let mut failed: i64 = 0;
    let mut by_name: std::collections::HashMap<String, (i64, i64, i64)> =
        std::collections::HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (tool, status, n) = row?;
        calls += n;
        let s = status.as_deref().unwrap_or("running");
        if s == "completed" {
            succeeded += n;
        } else if s == "error" {
            failed += n;
        }
        let e = by_name
            .entry(tool.unwrap_or_else(|| "?".into()))
            .or_insert((0, 0, 0));
        e.0 += n;
        if s == "completed" {
            e.1 += n;
        } else if s == "error" {
            e.2 += n;
        }
    }
    let unfinished = calls - succeeded - failed;
    let mut by_name_arr: Vec<serde_json::Value> = by_name
        .into_iter()
        .map(|(name, (c, s, f))| {
            serde_json::json!({"name": name, "calls": c, "succeeded": s, "failed": f,
                               "unfinished": c - s - f})
        })
        .collect();
    by_name_arr.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    // Day grain: one GROUP BY over msg time (bounded: distinct days).
    let mut stmt = conn.prepare_cached(
        "SELECT CAST(time_created / 86400000 AS INTEGER) AS day, count(*) \
         FROM msg GROUP BY day ORDER BY day",
    )?;
    let day_rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
    let mut days: Vec<(i64, i64)> = Vec::new();
    for row in day_rows {
        days.push(row?);
    }
    let active_days = days.len() as i64;
    // Streak: trailing consecutive UTC days ending today.
    let today = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_millis() / 86400000) as i64)
        .unwrap_or(0);
    let mut streak: i64 = 0;
    let mut want = today;
    for (d, _) in days.iter().rev() {
        if *d == want || (want == today && *d == today - 1) {
            // Allow "yesterday" as streak start when today is empty so a
            // daily user at 00:05 does not read streak 0.
            streak += 1;
            want = *d - 1;
        } else {
            break;
        }
    }
    let activity: Vec<serde_json::Value> = days
        .iter()
        .map(|(d, n)| serde_json::json!({"date": chrono_day(*d), "steps": n}))
        .collect();
    // Models: GROUP BY session.model (bounded: distinct models).
    let mut stmt = conn.prepare_cached(
        "SELECT model, count(*), COALESCE(SUM(tokens_input),0), COALESCE(SUM(tokens_output),0), \
                COALESCE(SUM(tokens_reasoning),0), COALESCE(SUM(cost),0.0) \
         FROM session GROUP BY model",
    )?;
    let model_rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, f64>(5)?,
        ))
    })?;
    let mut models: Vec<serde_json::Value> = Vec::new();
    for row in model_rows {
        let (model_json, steps, mi, mo, mr, mc) = row?;
        let (pid, mid) = match model_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        {
            Some(v) => (
                v.get("providerID")
                    .and_then(|x| x.as_str())
                    .unwrap_or("?")
                    .to_string(),
                v.get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("?")
                    .to_string(),
            ),
            None => ("?".into(), "?".into()),
        };
        models.push(serde_json::json!({
            "model": {"id": mid, "providerID": pid},
            "steps": steps,
            "tokens": {"input": mi, "output": mo, "reasoning": mr,
                       "cache": {"read": 0, "write": 0}},
            "cost": mc,
        }));
    }
    models.sort_by(|a, b| a["model"]["id"].as_str().cmp(&b["model"]["id"].as_str()));
    Ok(serde_json::json!({
        "range": {"from": tmin.unwrap_or(0), "to": tmax.unwrap_or(0)},
        "sessions": sessions,
        "subagents": 0,
        "prompts": prompts,
        "steps": steps,
        "tokens": {
            "input": tin, "output": tout, "reasoning": treason,
            "cache": {"read": tcr, "write": tcw},
        },
        "cost": tcost,
        "tools": {"mode": "summary",
                  "totals": {"calls": calls, "succeeded": succeeded,
                             "failed": failed, "unfinished": unfinished}},
        "activeDays": active_days,
        "streak": streak,
        "activity": activity,
        "models": models,
    }))
}

/// UTC day number → "YYYY-MM-DD" (stats activity axis; no chrono dep —
/// civil-from-days arithmetic).
fn chrono_day(day: i64) -> String {
    // Howard Hinnant's civil_from_days (days since 1970-01-01 → y/m/d).
    let z = day + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146097) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", y + if m <= 2 { 1 } else { 0 }, m, d)
}

/// v2 instruction entries (Phase D): content-addressed key journal over the
/// P2 `instruction_*` tables. Values store by sha256 in `instruction_blob`
/// (dedupe by construction); `instruction_entry` carries the key→hash link.
/// Explicit columns (rule 25/33).
pub mod instruction {
    use super::{WriteOp, Writer};
    use anyhow::Result;

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn hash(value: &str) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(value.as_bytes()))
    }

    /// Put a key (insert-or-replace the link + the blob). Value is caller JSON.
    pub fn put(writer: &Writer, session_id: &str, key: &str, value: &str) -> Result<()> {
        let h = hash(value);
        let now = now_ms();
        writer.write(vec![
            WriteOp::Sql {
                sql: "INSERT OR IGNORE INTO instruction_blob (hash, value) VALUES (?1, ?2)".into(),
                params: vec![h.clone().into(), value.into()],
            },
            WriteOp::Sql {
                sql: "INSERT INTO instruction_entry (session_id, key, value, removed, time_created, time_updated) \
                      VALUES (?1, ?2, ?3, 0, ?4, ?4) \
                      ON CONFLICT (session_id, key) DO UPDATE SET value = ?3, removed = 0, time_updated = ?4"
                    .into(),
                params: vec![session_id.into(), key.into(), h.into(), now.into()],
            },
        ])?;
        Ok(())
    }

    /// Remove a key (tombstone, not delete — the journal is append-biased;
    /// reset clears). Returns true when a live key transitioned.
    pub fn remove(writer: &Writer, session_id: &str, key: &str) -> Result<bool> {
        let n = writer.write(vec![WriteOp::Sql {
            sql: "UPDATE instruction_entry SET removed = 1, time_updated = ?3 \
                  WHERE session_id = ?1 AND key = ?2 AND removed = 0"
                .into(),
            params: vec![session_id.into(), key.into(), now_ms().into()],
        }])?;
        Ok(n == 1)
    }

    /// Live entries for a session (key → value JSON), oldest first.
    pub fn list(db: &std::path::Path, session_id: &str) -> Result<Vec<(String, String)>> {
        let conn = crate::pragma::open_reader(db)?;
        let mut stmt = conn.prepare_cached(
            "SELECT e.key, b.value FROM instruction_entry e \
             JOIN instruction_blob b ON b.hash = e.value \
             WHERE e.session_id = ?1 AND e.removed = 0 ORDER BY e.time_created ASC",
        )?;
        let rows = stmt.query_map([session_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/// v2 forms (V2-PLAN.md P4, L3-lite): persistent ask rendezvous state.
/// The waiters live in `core::form::FormGate`; this module is the durable
/// state (create/settle/get/list). Explicit columns (rule 25/33).
pub mod form {
    use super::{WriteOp, Writer};
    use anyhow::Result;

    /// One form row: (id, kind, title, fields_json, metadata_json, answered).
    pub type Row = (String, String, String, String, String, Option<String>);

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// Create a pending form. `fields`/`metadata` are caller JSON text.
    pub fn create(
        writer: &Writer,
        session_id: &str,
        id: &str,
        title: &str,
        metadata: &str,
        fields: &str,
    ) -> Result<()> {
        let now = now_ms();
        writer.write(vec![WriteOp::Sql {
            sql: "INSERT INTO form (id, session_id, title, metadata, fields, state, time_created, time_updated) \
                  VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6)"
                .into(),
            params: vec![
                id.into(),
                session_id.into(),
                title.into(),
                metadata.into(),
                fields.into(),
                now.into(),
            ],
        }])?;
        Ok(())
    }

    /// Settle a form (answered/cancelled). Answer JSON text or None.
    /// Returns `true` only when a pending row transitioned (re-settle is a
    /// no-op false — settled forms stay settled).
    pub fn settle(writer: &Writer, id: &str, state: &str, answer: Option<&str>) -> Result<bool> {
        let n = writer.write(vec![WriteOp::Sql {
            sql: "UPDATE form SET state = ?2, answer = ?3, time_updated = ?4 \
                  WHERE id = ?1 AND state = 'pending'"
                .into(),
            params: vec![id.into(), state.into(), answer.into(), now_ms().into()],
        }])?;
        Ok(n == 1)
    }

    /// One form by id (explicit columns).
    pub fn get(db: &std::path::Path, id: &str) -> Result<Option<Row>> {
        let conn = crate::pragma::open_reader(db)?;
        let mut stmt = conn.prepare_cached(
            "SELECT session_id, title, metadata, fields, state, answer FROM form WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        Ok(rows.next().transpose()?)
    }

    /// Pending forms for a session, oldest first.
    pub fn list_pending(
        db: &std::path::Path,
        session_id: &str,
    ) -> Result<Vec<(String, String, String, String)>> {
        let conn = crate::pragma::open_reader(db)?;
        let mut stmt = conn.prepare_cached(
            "SELECT id, title, metadata, fields FROM form \
             WHERE session_id = ?1 AND state = 'pending' ORDER BY time_created ASC",
        )?;
        let rows = stmt.query_map([session_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/// Events after a seq for durable replay (`log?after=` — V2-PLAN.md P4).
/// Explicit columns (rule 25/33); ascending seq; unbounded LIMIT is safe
/// here only because callers cap the session ring at EVENT_MAX_PER_SESSION
/// — the result is at most one ring's worth of rows.
pub fn events_after(
    db: &std::path::Path,
    session_id: &str,
    after: i64,
) -> anyhow::Result<Vec<(i64, String, String, i64)>> {
    let conn = pragma::open_reader(db)?;
    let mut stmt = conn.prepare_cached(
        "SELECT seq, type, payload, time_created FROM event \
         WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map((session_id, after), |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Load a session's durable events (newest first, capped) projected to the v2
/// `DurableEvent` shape: `{id,type,durable:{aggregateID,seq,version},data}`.
/// ocserve persists the v1 event envelope; this is a faithful re-projection
/// for `/api/session/{id}/history` (semantics are a subset of upstream's
/// full event-sourcing — documented divergence).
pub fn load_session_events(
    db: &std::path::Path,
    session_id: &str,
    limit: i64,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let conn = pragma::open_reader(db)?;
    let mut stmt = conn.prepare_cached(
        "SELECT seq, type, payload, time_created FROM event \
         WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map((session_id, limit), |r| {
        let seq: i64 = r.get(0)?;
        let ty: String = r.get(1)?;
        let payload: String = r.get(2)?;
        let _t: i64 = r.get(3)?;
        Ok((seq, ty, payload))
    })?;
    let mut out = Vec::new();
    for row in rows.flatten() {
        let (seq, ty, payload) = row;
        let data: serde_json::Value =
            serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
        // The stored payload is the v1 event envelope; its `properties` carry
        // the session-level event body when present. Fall back to the whole
        // payload so clients always receive an object.
        let body = data
            .pointer("/properties")
            .cloned()
            .unwrap_or_else(|| data.clone());
        // Stored types carry the durable version suffix (`type@v1`); the v2
        // projection strips it (the envelope's `version` carries it).
        let unversioned = unversioned_event_type(&ty);
        let version: i64 = ty
            .rsplit_once('@')
            .and_then(|(_, s)| s.strip_prefix('v')?.parse().ok())
            .unwrap_or(1);
        out.push(serde_json::json!({
            "id": format!("evt_{seq}"),
            "type": unversioned,
            "durable": {"aggregateID": session_id, "seq": seq, "version": version},
            "data": body,
        }));
    }
    Ok(out)
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
    maybe_prune_events(writer, now)
}

/// Throttled ring enforcement (STORAGE §4): every 1024th durable write
/// enqueues the retention deletes — an in-band bound, not a cron hope.
/// The cap-window DELETE is gated on an actual over-cap session (perf dive
/// 2026-10-11): the ROW_NUMBER window previously ran on every tick and
/// materialized the whole event table into a temp b-tree for nothing.
///
/// Shared by `append_event` and `emit_event` so the two emission paths keep
/// identical retention semantics.
fn maybe_prune_events(writer: &Writer, now: i64) -> anyhow::Result<()> {
    static EVENT_PRUNE_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    use std::sync::atomic::Ordering;
    if EVENT_PRUNE_TICK.fetch_add(1, Ordering::Relaxed) % 1024 == 1023 {
        writer.write(vec![WriteOp::Sql {
            sql: "DELETE FROM event WHERE time_created < ?1 AND time_created <> 0".into(),
            params: vec![(now - EVENT_MAX_AGE_MS).into()],
        }])?;
        let over: i64 = {
            let conn = crate::pragma::open_reader(writer.db_path_ref())?;
            conn.query_row(
                "SELECT count(*) FROM (SELECT 1 FROM event GROUP BY session_id HAVING count(*) > ?1 LIMIT 1)",
                [EVENT_MAX_PER_SESSION],
                |r| r.get(0),
            )?
        };
        if over > 0 {
            writer.write(vec![WriteOp::Sql {
                sql: "DELETE FROM event WHERE rowid IN (
                        SELECT rowid FROM (
                            SELECT rowid, ROW_NUMBER() OVER (
                                PARTITION BY session_id ORDER BY seq DESC
                            ) AS rn FROM event
                        ) WHERE rn > ?1
                    )"
                .into(),
                params: vec![EVENT_MAX_PER_SESSION.into()],
            }])?;
        }
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

/// The session's stored `directory` (the project root the client requested at
/// create time). Used to scope the prompt tool operations + the directory shown
/// on events to the session's own project rather than the server cwd — the
/// "session thinks it's in the ocserve repo" + constant external_directory
/// prompts class. `None` when the session is missing or has no directory.
pub fn session_directory(db: &std::path::Path, session_id: &str) -> anyhow::Result<Option<String>> {
    let conn = pragma::open_reader(db)?;
    let d: Option<String> = conn
        .query_row(
            "SELECT directory FROM session WHERE id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    Ok(d.filter(|s| !s.is_empty()))
}

/// A session's derived subagent ruleset (JSON array; v1 `{permission,
/// pattern, action}`) from the dedicated `subagent_rules` column, or None
/// when the session has none (F3: this is separate from `permission`, which
/// holds always-grants). Used by the `task` tool's child-context build.
pub fn session_permission(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let conn = pragma::open_reader(db)?;
    let raw: Option<Option<String>> = conn
        .query_row(
            "SELECT subagent_rules FROM session WHERE id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(raw
        .flatten()
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(&s).ok()))
}

/// The `parent_id` of a session, or None when it is a root (or unknown). Used
/// by the `task` tool's `task_id` lineage validation (K-TASK).
pub fn session_parent(db: &std::path::Path, session_id: &str) -> anyhow::Result<Option<String>> {
    let conn = pragma::open_reader(db)?;
    let raw: Option<Option<String>> = conn
        .query_row(
            "SELECT parent_id FROM session WHERE id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(raw.flatten().filter(|s| !s.is_empty()))
}

/// The `agent` of a session, or None when unset (or unknown). Used by the
/// `task` tool's `task_id` validation (K-TASK).
pub fn session_agent(db: &std::path::Path, session_id: &str) -> anyhow::Result<Option<String>> {
    let conn = pragma::open_reader(db)?;
    let raw: Option<Option<String>> = conn
        .query_row(
            "SELECT agent FROM session WHERE id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(raw.flatten().filter(|s| !s.is_empty()))
}

// ---------------------------------------------------------------------------
// Credentials (gap closure): upstream `credential` table semantics, verified
// live against 2.0.28 — create returns the entry (label defaults "default",
// active defaults true and single-actives same-integration siblings);
// activate flips one active + siblings inactive (per-integration scope,
// unknown id = 204 no-op); list echoes values in creation order (upstream
// parity — authenticated clients only). Explicit columns (rule 25/33).
// ---------------------------------------------------------------------------

/// One credential row (mirrors Credential.Entry wire shape).
pub struct Credential {
    pub id: String,
    pub integration_id: Option<String>,
    pub label: String,
    pub value: String,
    pub active: bool,
    pub time_created: i64,
    pub time_updated: i64,
}

impl Credential {
    pub fn to_wire(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "integrationID": self.integration_id,
            "label": self.label,
            "active": self.active,
            "value": serde_json::from_str::<serde_json::Value>(&self.value).unwrap_or(serde_json::Value::Null),
        })
    }
}

fn credential_row(r: &rusqlite::Row) -> rusqlite::Result<Credential> {
    Ok(Credential {
        id: r.get(0)?,
        integration_id: r.get(1)?,
        label: r.get(2)?,
        value: r.get(3)?,
        active: r.get::<_, i64>(4)? != 0,
        time_created: r.get(5)?,
        time_updated: r.get(6)?,
    })
}

/// List all credentials in creation order.
pub fn credential_list(db: &std::path::Path) -> anyhow::Result<Vec<Credential>> {
    let conn = pragma::open_reader(db)?;
    let mut stmt = conn.prepare_cached(
        "SELECT id, integration_id, label, value, active, time_created, time_updated \
         FROM credential ORDER BY time_created, rowid",
    )?;
    let rows = stmt.query_map([], credential_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Insert a credential; when `active`, same-integration siblings deactivate
/// (single-active per integration, verified live). Returns the entry.
pub fn credential_create(
    writer: &Writer,
    id: &str,
    integration_id: Option<&str>,
    label: &str,
    value: &str,
    active: bool,
    now_ms: i64,
) -> anyhow::Result<()> {
    let mut ops = vec![WriteOp::Sql {
        sql: "INSERT INTO credential \
              (id, integration_id, label, value, active, time_created, time_updated) \
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)"
            .into(),
        params: vec![
            id.into(),
            integration_id
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null),
            label.into(),
            value.into(),
            (if active { 1 } else { 0 }).into(),
            now_ms.into(),
        ],
    }];
    if active {
        ops.push(WriteOp::Sql {
            sql: "UPDATE credential SET active = 0, time_updated = ?2 \
                  WHERE integration_id IS ?1 AND id != ?3"
                .into(),
            params: vec![
                integration_id
                    .map(serde_json::Value::from)
                    .unwrap_or(serde_json::Value::Null),
                now_ms.into(),
                id.into(),
            ],
        });
    }
    writer.write(ops)?;
    Ok(())
}

/// Activate one credential (target active + siblings inactive); unknown id =
/// no-op Ok (upstream 204s unknown ids — idempotent, never 404).
pub fn credential_activate(
    writer: &Writer,
    db: &std::path::Path,
    id: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    let conn = pragma::open_reader(db)?;
    let row: Option<Option<String>> = conn
        .query_row(
            "SELECT integration_id FROM credential WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .ok();
    let Some(integration) = row else {
        return Ok(());
    };
    let flat: Option<String> = integration;
    writer.write(vec![
        WriteOp::Sql {
            sql: "UPDATE credential SET active = 0, time_updated = ?2 \
                  WHERE integration_id IS ?1 AND id != ?3"
                .into(),
            params: vec![
                flat.clone()
                    .map(serde_json::Value::from)
                    .unwrap_or(serde_json::Value::Null),
                now_ms.into(),
                id.into(),
            ],
        },
        WriteOp::Sql {
            sql: "UPDATE credential SET active = 1, time_updated = ?2 WHERE id = ?1".into(),
            params: vec![id.into(), now_ms.into()],
        },
    ])?;
    Ok(())
}

/// The ACTIVE credential value for an integration (websearch key lookup).
/// `None` when no active row exists.
pub fn credential_active_value(
    db: &std::path::Path,
    integration_id: &str,
) -> anyhow::Result<Option<String>> {
    let conn = pragma::open_reader(db)?;
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM credential \
             WHERE integration_id = ?1 AND active != 0 \
             ORDER BY time_updated DESC LIMIT 1",
            [integration_id],
            |r| r.get(0),
        )
        .ok();
    Ok(v)
}

// ---------------------------------------------------------------------------
// Session environment (gap closure, D-ENV): per-session string map, replaced
// wholesale by PUT. Read once per turn; injected into tool subprocess env.
// Never sent to the provider.
// ---------------------------------------------------------------------------

/// Replace the session's variables wholesale (REST replace semantics).
pub fn environment_put(
    writer: &Writer,
    session_id: &str,
    variables: &serde_json::Value,
    now_ms: i64,
) -> anyhow::Result<()> {
    writer.write(vec![WriteOp::Sql {
        sql: "INSERT INTO session_environment (session_id, variables, time_updated) \
              VALUES (?1, ?2, ?3) \
              ON CONFLICT(session_id) DO UPDATE SET variables = ?2, time_updated = ?3"
            .into(),
        params: vec![session_id.into(), variables.clone(), now_ms.into()],
    }])?;
    Ok(())
}

/// Load the session's variables (empty map when absent). Values are
/// strings by contract; non-string JSON coerces via to_string.
pub fn environment_get(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let conn = pragma::open_reader(db)?;
    let raw: Option<String> = conn
        .query_row(
            "SELECT variables FROM session_environment WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .ok();
    let mut out = Vec::new();
    if let Some(raw) = raw
        && let Ok(map) = serde_json::from_str::<serde_json::Value>(&raw)
        && let Some(obj) = map.as_object()
    {
        for (k, v) in obj {
            let s = v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string());
            out.push((k.clone(), s));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Set a session's directory (v2 `session.move` execution). Explicit columns
/// (rule 25/33); returns rows affected (0 = unknown session).
pub fn set_session_directory(
    writer: &Writer,
    session_id: &str,
    directory: &str,
) -> anyhow::Result<usize> {
    writer.write(vec![WriteOp::Sql {
        sql: "UPDATE session SET directory = ?2 WHERE id = ?1".into(),
        params: vec![session_id.into(), directory.into()],
    }])
}

/// All non-global, non-empty project `directory` values across sessions,
/// newest first. Feeds `GET /project` so the web UI can group sessions under
/// the directories they actually belong to (freeze derives its project list
/// from the sessions + worktree registry; ocserve derives it from stored
/// session rows — a documented derivation divergence, not a wire divergence).
pub fn distinct_session_directories(db: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let conn = pragma::open_reader(db)?;
    let mut stmt = conn.prepare_cached(
        "SELECT directory, MAX(time_updated) AS t FROM session
         WHERE directory IS NOT NULL AND directory != ''
         GROUP BY directory ORDER BY t DESC LIMIT 200",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
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

/// Insert a SUBAGENT (task-tool) child session: parent_id, agent, and the
/// derived permission ruleset (JSON array, v1 `{permission,pattern,action}`)
/// stored in the dedicated `subagent_rules` column (F3: the `permission`
/// column is reserved for always-grants and must not be overwritten). Used
/// only by the `task` tool launcher (K-TASK).
pub fn insert_child_session(
    writer: &Writer,
    info: &serde_json::Value,
    parent_id: &str,
    agent: &str,
    permission: &serde_json::Value,
) -> anyhow::Result<()> {
    let id = info["id"].as_str().unwrap_or_default().to_string();
    writer.write(vec![WriteOp::Sql {
        sql: "INSERT OR REPLACE INTO session (id, project_id, slug, directory, path, title, version, \
              time_created, time_updated, parent_id, agent, subagent_rules) \
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
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
            parent_id.into(),
            agent.into(),
            permission.to_string().into(),
        ],
    }])?;
    Ok(())
}

/// Walk the `parent_id` chain from `session_id`; returns the number of
/// ancestors (depth). A root session → 0. Missing session → None. Used by the
/// `task` tool's `subagent_depth` guard (K-TASK).
pub fn session_depth(db: &std::path::Path, session_id: &str) -> anyhow::Result<Option<u32>> {
    let conn = pragma::open_reader(db)?;
    let mut depth = 0u32;
    let mut cur = session_id.to_string();
    loop {
        let next: Option<Option<String>> = conn
            .query_row("SELECT parent_id FROM session WHERE id = ?1", [&cur], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?;
        match next {
            None => return Ok(None), // session id unknown
            Some(None) => return Ok(Some(depth)),
            Some(Some(p)) => {
                depth += 1;
                if depth > 64 {
                    return Ok(Some(depth)); // cycle guard (should never happen)
                }
                cur = p;
            }
        }
    }
}

/// Full session wire object merged with `overrides` (upstream `patch()`:
/// `{...current, ...info}`). Used by every `session.updated` emit so the event
/// ALWAYS carries the full Session (spec: required id/slug/projectID/directory/
/// title/version/time) — a partial `info` merges into client stores and crashes
/// renderers that read `title`/`time` (TUI `r.title.length`, 2026-10-09).
///
/// `overrides` supplies values not yet persisted (mid-prompt cost/tokens land
/// on the row only at finalize). `time` is deep-merged (upstream
/// `{...current.time, ...info.time}`) so a `{updated}` override keeps
/// `created`. Missing session → `None`.
pub fn session_wire_with(
    db: &std::path::Path,
    session_id: &str,
    overrides: serde_json::Value,
) -> anyhow::Result<Option<serde_json::Value>> {
    let Some(mut base) = load_session_wire(db, session_id)? else {
        return Ok(None);
    };
    if let (serde_json::Value::Object(b), serde_json::Value::Object(o)) = (&mut base, overrides) {
        for (k, v) in o {
            if k == "time"
                && let (Some(serde_json::Value::Object(bt)), serde_json::Value::Object(ot)) =
                    (b.get_mut("time"), &v)
            {
                for (tk, tv) in ot {
                    bt.insert(tk.clone(), tv.clone());
                }
                continue;
            }
            b.insert(k, v);
        }
    }
    Ok(Some(base))
}

/// One session in upstream wire shape (list schema — always includes agent/
/// model/summary keys, null when unset; create-response is a leaner shape).
pub fn load_session_wire(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let conn = pragma::open_reader(db)?;
    let mut stmt = conn.prepare_cached(
        "SELECT id, project_id, directory, path, slug, title, version, agent, model, cost,
                summary_additions, summary_deletions, summary_files,
                tokens_input, tokens_output, tokens_reasoning,
                tokens_cache_read, tokens_cache_write, time_created, time_updated, revert
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
            "revert": r.get::<_, Option<String>>(20)?
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .unwrap_or(serde_json::Value::Null),
        }))
    })?;
    Ok(rows.filter_map(|r| r.ok()).next())
}

/// K-REVERT: set/clear the session `revert` marker (stored as JSON text).
pub fn set_session_revert(
    writer: &Writer,
    session_id: &str,
    revert: Option<&serde_json::Value>,
) -> anyhow::Result<usize> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    match revert {
        Some(v) => writer.write(vec![WriteOp::Sql {
            sql: "UPDATE session SET revert = ?2, time_updated = ?3 WHERE id = ?1".into(),
            params: vec![session_id.into(), v.to_string().into(), now.into()],
        }]),
        None => writer.write(vec![WriteOp::Sql {
            sql: "UPDATE session SET revert = NULL, time_updated = ?2 WHERE id = ?1".into(),
            params: vec![session_id.into(), now.into()],
        }]),
    }
}

/// Lazy revert cleanup (upstream `SessionRevert.cleanup`, called at prompt
/// start `prompt.ts:459/1056`): a session carrying a `revert` marker drops the
/// reverted range (messages from the target onward, or the target's parts from
/// `partID` onward) and clears the marker. No-op without a marker.
pub fn revert_cleanup(
    writer: &Writer,
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<()> {
    let Some(sess) = load_session_wire(db, session_id)? else {
        return Ok(());
    };
    let Some(rev) = sess.get("revert") else {
        return Ok(());
    };
    if rev.is_null() {
        return Ok(());
    }
    let mid = rev.get("messageID").and_then(|v| v.as_str()).unwrap_or("");
    if mid.is_empty() {
        return Ok(());
    }
    let pid = rev.get("partID").and_then(|v| v.as_str());
    let from = messages_from(db, session_id, mid)?; // inclusive, chronological
    match pid {
        Some(pid) => {
            // keep the target message; delete its parts from `partID` onward,
            // then every message strictly after the target.
            let msgs = load_messages(db, session_id, None)?;
            if let Some((_info, parts)) = msgs.iter().find(|(i, _)| i["id"].as_str() == Some(mid)) {
                let mut drop = false;
                for p in parts {
                    if p["id"].as_str() == Some(pid) {
                        drop = true;
                    }
                    if drop && let Some(pidv) = p["id"].as_str() {
                        delete_part(writer, db, mid, pidv)?;
                    }
                }
            }
            for m in from.iter().skip(1) {
                delete_message(writer, db, session_id, m)?;
            }
        }
        None => {
            for m in &from {
                delete_message(writer, db, session_id, m)?;
            }
        }
    }
    set_session_revert(writer, session_id, None)?;
    Ok(())
}

/// Messages from `message_id` onward (revert range computation; v1 slices
/// the message list from the revert target's index).
pub fn messages_from(
    db: &std::path::Path,
    session_id: &str,
    message_id: &str,
) -> anyhow::Result<Vec<String>> {
    let conn = pragma::open_reader(db)?;
    let target: Option<i64> = conn
        .query_row(
            "SELECT time_created FROM msg WHERE id = ?1 AND session_id = ?2",
            (message_id, session_id),
            |r| r.get(0),
        )
        .ok();
    let Some(t) = target else { return Ok(vec![]) };
    let mut stmt = conn.prepare_cached(
        "SELECT id FROM msg WHERE session_id = ?1 AND (time_created > ?2 OR (time_created = ?2 AND id >= ?3)) \
         ORDER BY time_created, id",
    )?;
    let out = stmt
        .query_map((session_id, t, message_id), |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(out)
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
/// metadata/permission/archived fields are not stored by ocserve — ignored).
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

// ---- K-ALWAYS / K-TITLE / K-AUTONOMY: session permission persistence,
// default-title retag, first-user-text extraction, turn-stop surfacing ----

/// K-MODEL-STATE: persist resolved (agent, model) at PROMPT START —
/// survives cap/OOM/abort deaths that never reach finalize (the "model
/// reverts" bug). Churn-free: identical values affect 0 rows.
pub fn persist_prompt_model(
    writer: &Writer,
    session_id: &str,
    agent: &str,
    model_json: &str,
) -> anyhow::Result<usize> {
    writer.write(vec![WriteOp::Sql {
        sql: "UPDATE session SET agent = ?2, model = ?3 \
              WHERE id = ?1 AND (agent IS NOT ?2 OR model IS NOT ?3)"
            .into(),
        params: vec![session_id.into(), agent.into(), model_json.into()],
    }])
}

/// Persisted "always" grants for a session (column `permission`, JSON array of
/// `"<permission>\t<pattern>"` strings). Missing row/column → empty.
pub fn session_always_keys(db: &std::path::Path, session_id: &str) -> anyhow::Result<Vec<String>> {
    let conn = pragma::open_reader(db)?;
    let raw: Option<Option<String>> = conn
        .query_row(
            "SELECT permission FROM session WHERE id = ?1",
            [session_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    match raw.flatten() {
        Some(t) if !t.is_empty() => Ok(serde_json::from_str(&t).unwrap_or_default()),
        _ => Ok(Vec::new()),
    }
}

/// Append a granted-always rule (idempotent; prompt lock serializes writers).
/// Stored as `"<permission>\t<pattern>"` (tab keeps patterns with spaces, e.g.
/// `git status *`, unambiguous).
pub fn session_grant_always(
    writer: &Writer,
    db: &std::path::Path,
    session_id: &str,
    permission: &str,
    pattern: &str,
) -> anyhow::Result<()> {
    let key = format!("{permission}\t{pattern}");
    let mut keys = session_always_keys(db, session_id)?;
    if keys.iter().any(|k| k == &key) {
        return Ok(());
    }
    keys.push(key);
    writer.write(vec![WriteOp::Sql {
        sql: "UPDATE session SET permission = ?2 WHERE id = ?1".into(),
        params: vec![session_id.into(), serde_json::to_string(&keys)?.into()],
    }])?;
    Ok(())
}

/// Retag only DEFAULT-titled sessions (freeze default `New session - …` or
/// the pre-fix empty title); named sessions are never touched (row count 0).
/// Bumps time_updated like update_session_title (list re-sorts on rename).
pub fn retag_default_title(writer: &Writer, session_id: &str, title: &str) -> anyhow::Result<bool> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let n = writer.write(vec![WriteOp::Sql {
        sql: "UPDATE session SET title = ?2, time_updated = ?3 \
              WHERE id = ?1 AND (title = '' OR title LIKE 'New session - %')"
            .into(),
        params: vec![session_id.into(), title.into(), now.into()],
    }])?;
    Ok(n > 0)
}

/// First user message's first text part (auto-title source). Blobbed texts
/// are materialized via the blob store; unresolvable → None.
pub fn first_user_text(
    db: &std::path::Path,
    session_id: &str,
    blobs: Option<&BlobStore>,
) -> anyhow::Result<Option<String>> {
    let conn = pragma::open_reader(db)?;
    let row: Option<(Option<String>, Option<String>, i64)> = conn
        .query_row(
            "SELECT p.inline, p.blob_sha, p.byte_len FROM msg m \
             JOIN msg_part p ON p.message_id = m.id \
             WHERE m.session_id = ?1 AND m.role = 'user' AND p.type = 'text' \
             ORDER BY m.seq, p.seq LIMIT 1",
            [session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((inline, sha, byte_len)) = row else {
        return Ok(None);
    };
    let raw = match (inline, sha) {
        (Some(t), _) => t,
        (None, Some(sha)) => {
            let Some(store) = blobs else { return Ok(None) };
            let bytes = store.get(&sha, byte_len as u64)?;
            String::from_utf8_lossy(&bytes).into_owned()
        }
        (None, None) => return Ok(None),
    };
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    Ok(v.get("text").and_then(|t| t.as_str()).map(String::from))
}

/// K-AUTONOMY surfacing: append a durable `[turn stopped] …` text part to
/// the run's LAST assistant message (and mark it completed when it wasn't).
/// Returns the built part for the SSE fan-out, or None when the run died
/// before any assistant message existed (session.error still fires).
/// All msg_part writes stay here with their part_search companion (rule 4).
pub fn mark_turn_stopped(
    writer: &Writer,
    db: &std::path::Path,
    session_id: &str,
    part_id: &str,
    reason: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let last: Option<(String, Option<i64>)> = {
        let conn = pragma::open_reader(db)?;
        conn.query_row(
            "SELECT id, json_extract(info, '$.time.completed') FROM msg \
             WHERE session_id = ?1 AND role = 'assistant' ORDER BY seq DESC LIMIT 1",
            [session_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
        )
        .optional()?
    };
    let Some((mid, completed)) = last else {
        return Ok(None);
    };
    let part = serde_json::json!({
        "id": part_id,
        "sessionID": session_id,
        "messageID": mid,
        "type": "text",
        "text": format!("[turn stopped] {reason}"),
    });
    let text = part.to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut ops = Vec::new();
    if completed.is_none() {
        ops.push(WriteOp::Sql {
            sql: "UPDATE msg SET info = json_set(info, '$.time.completed', ?2) \
                  WHERE id = ?1 AND json_extract(info, '$.time.completed') IS NULL"
                .into(),
            params: vec![mid.clone().into(), now.into()],
        });
    }
    ops.push(WriteOp::Sql {
        sql: "INSERT OR REPLACE INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) \
              VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(seq),0)+1 FROM msg_part WHERE message_id=?2), 'text', ?4, ?5, NULL)"
            .into(),
        params: vec![
            part_id.into(),
            mid.clone().into(),
            session_id.into(),
            (text.len() as i64).into(),
            text.clone().into(),
        ],
    });
    ops.push(part_search_upsert_ops(part_id, session_id, &mid, &text));
    writer.write(ops)?;
    Ok(Some(part))
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

/// GET /session/{id}/children — sessions whose parent_id is this id (ocserve
/// never sets parent_id today → always []; shape-correct for the client).
pub fn load_children(
    db: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let conn = pragma::open_reader(db)?;
    let mut stmt = conn
        .prepare_cached("SELECT id FROM session WHERE parent_id = ?1 ORDER BY time_updated DESC")?;
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
    let mut stmt = conn.prepare_cached(
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
    session_id: &str,
    message_id: &str,
    part_id: &str,
    data: &serde_json::Value,
) -> anyhow::Result<bool> {
    let text = data.to_string();
    let byte_len = text.len() as i64;
    let upsert = part_search_upsert_ops(part_id, session_id, message_id, &text);
    let (inline, sha) = if text.len() > INLINE_PART_MAX {
        let (sha, _, _) = blobs.put(text.as_bytes())?;
        (None, Some(sha))
    } else {
        (Some(text), None)
    };
    let mut ops = vec![
        WriteOp::Sql {
            sql: "UPDATE msg_part SET byte_len = ?3, inline = ?4, blob_sha = ?5 WHERE id = ?1 AND message_id = ?2"
                .into(),
            params: vec![
                part_id.into(),
                message_id.into(),
                byte_len.into(),
                inline.into(),
                sha.into(),
            ],
        },
        upsert,
    ];
    if data["type"] == "compaction" {
        ops.push(compaction_tail_ops(part_id, data));
    }
    let n = writer.write(ops)?;
    Ok(n > 0)
}

/// End-of-prompt session row update (agent/model/cost/tokens/time).
/// Single-line SQL (a `\` line-continuation once produced a literal backslash
/// → syntax error → was swallowed by `let _ =` — AGENTS §2.5 forbids the
/// swallow; this fn propagates and is covered by a row-affecting test).
/// Session-usage accumulation (upstream `applyUsage`,
/// core/session/projector.ts:89-108): EVERY `step-finish` part ADDS its usage
/// to the session row, and a part removal SUBTRACTS it. ocserve accumulates at
/// persist time instead of a projection pass — same arithmetic, one writer
/// batch. `tokens` is an assistant-message `tokens` object (input/output/
/// reasoning/cache.read/cache.write); `total` is ignored (session shape has no
/// total).
pub fn accumulate_session_usage(
    writer: &Writer,
    session_id: &str,
    tokens: &serde_json::Value,
    cost: f64,
) -> anyhow::Result<()> {
    let get = |p: &str| tokens.pointer(p).and_then(|v| v.as_i64()).unwrap_or(0);
    writer.write(vec![WriteOp::Sql {
        sql: "UPDATE session SET cost = cost + ?2, \
              tokens_input = tokens_input + ?3, \
              tokens_output = tokens_output + ?4, \
              tokens_reasoning = tokens_reasoning + ?5, \
              tokens_cache_read = tokens_cache_read + ?6, \
              tokens_cache_write = tokens_cache_write + ?7 \
              WHERE id = ?1"
            .into(),
        params: vec![
            session_id.into(),
            cost.into(),
            get("/input").into(),
            get("/output").into(),
            get("/reasoning").into(),
            get("/cache/read").into(),
            get("/cache/write").into(),
        ],
    }])?;
    Ok(())
}

pub struct PromptStats<'a> {
    pub agent: &'a str,
    pub model_json: &'a str,
    pub time_updated: i64,
}

/// Finalize writes ONLY agent/model/time_updated. Cost and tokens are NOT
/// written here: the session row accumulates lifetime usage per step-finish
/// (upstream applyUsage), so overwriting with any turn-scoped total would
/// clobber other turns' contribution (the 869%-meter bug: the row carried only
/// the last turn's step sum, and the final MESSAGE carried the whole-turn sum).
pub fn finalize_session_prompt(
    writer: &Writer,
    session_id: &str,
    stats: &PromptStats<'_>,
) -> anyhow::Result<usize> {
    writer.write(vec![WriteOp::Sql {
        sql: "UPDATE session SET agent = ?2, model = ?3, time_updated = ?4 WHERE id = ?1".into(),
        params: vec![
            session_id.into(),
            stats.agent.into(),
            stats.model_json.into(),
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
            let mut stmt = conn.prepare_cached(
                "SELECT id, time_created, data FROM message \
                 WHERE session_id = ?1 AND (time_created > ?2 OR (time_created = ?2 AND id > ?3)) \
                 ORDER BY time_created, id LIMIT ?4",
            )?;
            stmt.query_map((session_id, ctime, cid, cap as i64 + 1), row_to_legacy)?
                .filter_map(|r| r.ok())
                .collect()
        }
        None => {
            let mut stmt = conn.prepare_cached(
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

/// (part_id, message_id, session_id, inline, blob_sha)
type PartSrcRow = (String, String, String, Option<String>, Option<String>, i64);

/// One-time part content backfill (W1): index every msg_part not yet in
/// part_search. Idempotent (`ON CONFLICT` upsert + NOT EXISTS selection) so
/// an interrupted run resumes; blobbed parts decompress through the blob
/// store. Returns (indexed, skipped, elapsed_ms).
/// Stamp the parts-index parity marker (boot-cost gate, perf dive 2026-10-11):
/// written after any operation that makes `part_search_fts` complete w.r.t.
/// `part_search` (import finalize, boot backfill, self-heal rebuild). The boot
/// parity check then skips the two O(N) table counts when
/// stamp == docsize && trigger present.
pub fn stamp_parts_indexed(writer: &Writer, count: i64) -> anyhow::Result<()> {
    writer.write(vec![crate::WriteOp::Sql {
        sql: "INSERT OR REPLACE INTO import_manifest (key, value) VALUES ('parts_indexed', ?1)"
            .into(),
        params: vec![count.into()],
    }])?;
    Ok(())
}

pub fn backfill_part_search(
    writer: &Writer,
    db: &std::path::Path,
) -> anyhow::Result<(u64, u64, u64)> {
    let t0 = std::time::Instant::now();
    let conn = pragma::open_reader(db)?;
    // Boot-cost gate (perf dive 2026-10-11): the healthy-path parity check
    // counted BOTH msg_part and part_search in full — index-only `count(*)`
    // scans measured 2.7-4.4 s + 1.5-3.2 s cold on the live 177k-part DB on
    // EVERY boot. A stamped manifest key (written by the import finalize +
    // after every self-heal repair) converts the healthy path to a
    // single-row key lookup; the counts only run when the stamp is absent
    // (pre-stamp databases) or when the stamp fails to match the cheap
    // docsize check (corruption/crash evidence), so nothing is skipped.
    let stamped: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM import_manifest WHERE key = 'parts_indexed'",
            [],
            |r| r.get(0),
        )
        .ok();
    let trigger: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name='part_search_ai'",
        [],
        |r| r.get(0),
    )?;
    let fts_docs: i64 = conn
        .query_row("SELECT count(*) FROM part_search_fts_docsize", [], |r| {
            r.get(0)
        })
        .unwrap_or(-1);
    if let Some(n) = stamped
        && trigger == 1
        && fts_docs >= 0
        && fts_docs as i64 == n
    {
        drop(conn);
        return Ok((0, 0, t0.elapsed().as_millis() as u64));
    }
    let total: i64 = conn.query_row("SELECT count(*) FROM msg_part", [], |r| r.get(0))?;
    let have: i64 = conn.query_row("SELECT count(*) FROM part_search", [], |r| r.get(0))?;
    if have >= total {
        drop(conn);
        // repair only when needed — a healthy DB pays the stamp lookup above
        // and lands here only when the stamp is missing or mismatched
        if trigger == 0 || (fts_docs >= 0 && fts_docs < have) {
            let wconn = crate::pragma::open_writer(db)?;
            if trigger == 0 {
                tracing::warn!(
                    "part_search FTS trigger missing (interrupted import?) — recreating"
                );
                wconn.execute_batch(
                    "CREATE TRIGGER part_search_ai AFTER INSERT ON part_search BEGIN
                         INSERT INTO part_search_fts(rowid, text) VALUES (new.rowid, new.text);
                     END",
                )?;
            }
            if fts_docs >= 0 && fts_docs < have {
                tracing::warn!(
                    "part_search FTS index incomplete ({fts_docs}/{have} docs) — bulk rebuild"
                );
                let t_rb = std::time::Instant::now();
                wconn.execute_batch(
                    "INSERT INTO part_search_fts(part_search_fts) VALUES('rebuild')",
                )?;
                tracing::warn!(
                    "part_search FTS rebuild took {:.1}s",
                    t_rb.elapsed().as_secs_f64()
                );
            }
            drop(wconn);
        }
        // healthy (already or just repaired) — stamp so future boots skip
        // the O(N) counts entirely
        let _ = stamp_parts_indexed(writer, have);
        return Ok((0, 0, t0.elapsed().as_millis() as u64));
    }
    let blobs = crate::blob::BlobStore::new(
        db.parent()
            .ok_or_else(|| anyhow::anyhow!("db parent"))?
            .join("blobs"),
    )?;
    let mut stmt = conn.prepare(
        "SELECT id, message_id, session_id, inline, blob_sha, byte_len FROM msg_part p WHERE NOT EXISTS (SELECT 1 FROM part_search ps WHERE ps.part_id = p.id) ORDER BY session_id, message_id, seq",
    )?;
    let rows: Vec<PartSrcRow> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);
    drop(conn);
    let mut indexed = 0u64;
    let mut skipped = 0u64;
    let mut batch: Vec<WriteOp> = Vec::with_capacity(200);
    for (pid, mid, sid, inline, sha, byte_len) in rows {
        let text = match inline {
            Some(t) => t,
            None => {
                let Some(sha) = sha else {
                    skipped += 1;
                    continue;
                };
                match blobs.get(&sha, byte_len.max(0) as u64) {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(t) => t,
                        Err(_) => {
                            skipped += 1;
                            continue;
                        }
                    },
                    Err(e) => {
                        tracing::warn!("backfill blob {sha}: {e:#}");
                        skipped += 1;
                        continue;
                    }
                }
            }
        };
        batch.push(part_search_upsert_ops(&pid, &sid, &mid, &text));
        indexed += 1;
        if batch.len() >= 200 {
            writer.write(std::mem::take(&mut batch))?;
        }
    }
    if !batch.is_empty() {
        writer.write(batch)?;
    }
    if indexed > 0 {
        let _ = stamp_parts_indexed(writer, have + indexed as i64);
    }
    Ok((indexed, skipped, t0.elapsed().as_millis() as u64))
}

/// Search hit (W1 contract — PLAN §17 block).
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub session_id: String,
    pub message_id: String,
    pub part_id: String,
    pub role: String,
    pub time: i64,
    pub text: String,
}

/// Content search over part payloads. ≥3 chars → trigram FTS MATCH (quoted
/// phrase, substring semantics); shorter → LIKE fallback (same projection).
/// One SQL statement; `limit+1` probe reports `truncated`.
// ---- PERF-10X F7/F8: epoch-keyed memoization (exact, single-writer) ----
//
// Single writer => a monotonically bumped WRITE_EPOCH (bumped after every
// committed writer batch and every apply_ops batch, eager-cleared) makes
// lookups EXACTLY correct: an entry is only served when its epoch equals
// the live epoch, and a query that raced a write is discarded rather than
// stored (e1 != e0). No TTL, no staleness window, no LRU revalidation.
// Caps: 64 search entries (FIFO) + 1 list entry; kill switches
// OCSERVE_SEARCH_MEMO=0 / OCSERVE_LIST_MEMO=0. Benchmark reports MUST carry
// cache-on and cache-off variants (PERF-10X ethics): k6 repeats one query,
// which overstates the hit rate vs real traffic.
static WRITE_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SEARCH_MEMO_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SEARCH_MEMO_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LIST_MEMO_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LIST_MEMO_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub const SEARCH_MEMO_CAP: usize = 64;

/// Live write epoch (Acquire).
pub fn write_epoch() -> u64 {
    WRITE_EPOCH.load(std::sync::atomic::Ordering::Acquire)
}

/// Bump after a committed write batch + eagerly clear both memos (memory
/// released immediately; lazy epoch checks remain the correctness backstop).
pub fn bump_write_epoch() {
    WRITE_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Release);
    let mut s = search_memo().lock();
    s.clear();
    let mut l = list_memo().lock();
    l.slot = None;
    l.bytes = None;
    l.epoch = 0;
    l.db = None;
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct SearchKey {
    needle: String,
    scope: Option<String>,
    limit: u32,
    offset: u32,
}

/// Cached result: hits + truncated flag, shared so a hit is an Arc clone.
type CachedHits = std::sync::Arc<(Vec<SearchHit>, bool)>;

struct SearchMemoState {
    epoch: u64,
    /// See `ListMemoState::db` — same identity rule: a memo entry is only
    /// served to the db that produced it, otherwise the table is cleared.
    db: Option<std::path::PathBuf>,
    entries: std::collections::VecDeque<(SearchKey, CachedHits)>,
}

impl SearchMemoState {
    fn clear(&mut self) {
        self.entries.clear();
        self.db = None;
    }
    fn get(
        &mut self,
        key: &SearchKey,
        epoch: u64,
        db: &std::path::Path,
    ) -> Option<std::sync::Arc<(Vec<SearchHit>, bool)>> {
        if self.epoch != epoch {
            self.entries.clear();
            self.epoch = epoch;
            self.db = None;
        }
        // identity: same db, or nothing was ever cached (db set on first put)
        if self.db.as_deref().is_some_and(|d| d != db) {
            self.entries.clear();
            self.db = None;
        }
        // FIFO walk (cap 64 — linear scan of ≤64 short keys, cheaper than a
        // HashMap upkeep; keys are workload-stable)
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| std::sync::Arc::clone(v))
    }
    fn put(&mut self, key: SearchKey, val: CachedHits, epoch: u64, db: &std::path::Path) {
        if self.epoch != epoch {
            self.entries.clear();
            self.epoch = epoch;
        }
        if self.db.as_deref().is_some_and(|d| d != db) {
            self.entries.clear();
            self.db = None;
        }
        if self.db.is_none() {
            self.db = Some(db.to_path_buf());
        }
        self.entries.retain(|(k, _)| k != &key);
        self.entries.push_back((key, val));
        while self.entries.len() > SEARCH_MEMO_CAP {
            self.entries.pop_front();
        }
    }
    fn len(&self) -> usize {
        self.entries.len()
    }
}

struct ListMemoState {
    epoch: u64,
    /// DB identity this slot was filled from. The epoch alone is NOT an
    /// identity: `write_epoch` is process-global, so two different databases
    /// in one process (tests, or any future multi-db reader) can share an
    /// epoch and silently serve each other's bytes. Found 2026-10-07 when the
    /// corpus parity gate ran two dbs in one test binary and the synthetic
    /// test received the live db's bytes. A hit now requires BOTH epoch and
    /// path match; a path mismatch clears the slot rather than serving it.
    db: Option<std::path::PathBuf>,
    slot: Option<std::sync::Arc<Vec<serde_json::Value>>>,
    /// Serialized wire bytes of the same list (F5-extends-F8): a hit is a
    /// refcounted Bytes clone — no per-request 300KB serde pass.
    bytes: Option<std::sync::Arc<bytes::Bytes>>,
}

fn search_memo() -> &'static parking_lot::Mutex<SearchMemoState> {
    static M: std::sync::OnceLock<parking_lot::Mutex<SearchMemoState>> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        parking_lot::Mutex::new(SearchMemoState {
            epoch: 0,
            db: None,
            entries: std::collections::VecDeque::new(),
        })
    })
}

/// W4: bounded param-keyed memo for filtered session lists (cap 16 keys;
/// bare list keeps its own exact single slot). Epoch+db identity like the
/// search memo.
struct FilteredListMemo {
    epoch: u64,
    db: Option<std::path::PathBuf>,
    entries: std::collections::VecDeque<(String, std::sync::Arc<bytes::Bytes>)>,
}

const FILTERED_LIST_CAP: usize = 16;

impl FilteredListMemo {
    fn get(
        &mut self,
        key: &str,
        epoch: u64,
        db: &std::path::Path,
    ) -> Option<std::sync::Arc<bytes::Bytes>> {
        if self.epoch != epoch {
            self.entries.clear();
            self.epoch = epoch;
            self.db = None;
        }
        if self.db.as_deref().is_some_and(|d| d != db) {
            self.entries.clear();
            self.db = None;
        }
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
    fn put(
        &mut self,
        key: String,
        val: std::sync::Arc<bytes::Bytes>,
        epoch: u64,
        db: &std::path::Path,
    ) {
        if self.epoch != epoch {
            self.entries.clear();
            self.epoch = epoch;
        }
        if self.db.as_deref().is_some_and(|d| d != db) {
            self.entries.clear();
            self.db = None;
        }
        if self.db.is_none() {
            self.db = Some(db.to_path_buf());
        }
        self.entries.retain(|(k, _)| k != &key);
        self.entries.push_back((key, val));
        while self.entries.len() > FILTERED_LIST_CAP {
            self.entries.pop_front();
        }
    }
}

fn filtered_memo() -> &'static parking_lot::Mutex<FilteredListMemo> {
    static M: std::sync::OnceLock<parking_lot::Mutex<FilteredListMemo>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| {
        parking_lot::Mutex::new(FilteredListMemo {
            epoch: 0,
            db: None,
            entries: std::collections::VecDeque::new(),
        })
    })
}

fn list_memo() -> &'static parking_lot::Mutex<ListMemoState> {
    static M: std::sync::OnceLock<parking_lot::Mutex<ListMemoState>> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        parking_lot::Mutex::new(ListMemoState {
            epoch: 0,
            db: None,
            slot: None,
            bytes: None,
        })
    })
}

/// Kill-switch semantics as a PURE fn (testable without env mutation):
/// explicit override wins; otherwise env "0" disables, anything else (or
/// unset) enables.
fn memo_enabled(ovr: Option<bool>, env: Option<&str>) -> bool {
    ovr.unwrap_or_else(|| env.is_none_or(|v| v != "0"))
}

pub fn search_memo_enabled() -> bool {
    memo_enabled(None, std::env::var("OCSERVE_SEARCH_MEMO").ok().as_deref())
}

pub fn list_memo_enabled() -> bool {
    memo_enabled(None, std::env::var("OCSERVE_LIST_MEMO").ok().as_deref())
}

/// Observable counters for tests + the load report (hit rate per variant).
pub fn memo_stats() -> (u64, u64, u64, u64, u64, usize, u64) {
    (
        SEARCH_MEMO_HITS.load(std::sync::atomic::Ordering::Relaxed),
        SEARCH_MEMO_MISSES.load(std::sync::atomic::Ordering::Relaxed),
        LIST_MEMO_HITS.load(std::sync::atomic::Ordering::Relaxed),
        LIST_MEMO_MISSES.load(std::sync::atomic::Ordering::Relaxed),
        0,
        search_memo().lock().len(),
        write_epoch(),
    )
}

// Search SQL shapes (PERF-10X S-A). All single-line (check-guards rule 2).
// Global match: fts rowid-DESC walk + LIMIT inside the subquery (early
// termination), payload+msg join only for the <=limit+1 survivors.
const SEARCH_MATCH_GLOBAL: &str = "SELECT ps.session_id, ps.message_id, ps.part_id, m.role, m.time_created, ps.text FROM part_search ps JOIN msg m ON m.id = ps.message_id WHERE ps.rowid IN (SELECT rowid FROM part_search_fts WHERE part_search_fts MATCH ?1 ORDER BY rowid DESC LIMIT ?2 OFFSET ?3) ORDER BY ps.rowid DESC";
// Scoped match (perf dive 2026-10-11): the pre-fix shape walked the session's
// rowid range with a per-row FTS EXISTS probe — no early termination, so an
// absent (or old) needle scanned every row of the session: measured 215,742 ms
// on the 56k-row session (97.7s warm), reproduced live. The S3 shape drives
// from the vtab (SCAN f VIRTUAL TABLE early-terminates after LIMIT+1 hits),
// probes part_search by rowid, filters session_id in the residual, and keeps
// the same ORDER BY f.rowid DESC contract: measured 5 ms on the same needle,
// 395 ms worst case on the densest trigram ("the"), page-by-page equal to
// global-filtered results (proven in the E2 shootout before adoption).
const SEARCH_MATCH_SCOPED: &str = "SELECT ps.session_id, ps.message_id, ps.part_id, m.role, m.time_created, ps.text FROM part_search_fts f JOIN part_search ps ON ps.rowid = f.rowid JOIN msg m ON m.id = ps.message_id WHERE part_search_fts MATCH ?1 AND ps.session_id = ?2 ORDER BY f.rowid DESC LIMIT ?3 OFFSET ?4";
// LIKE fallback (<3 chars): rowid-DESC scan with LIMIT — no sort needed.
const SEARCH_LIKE_GLOBAL: &str = "SELECT ps.session_id, ps.message_id, ps.part_id, m.role, m.time_created, ps.text FROM part_search ps JOIN msg m ON m.id = ps.message_id WHERE ps.text LIKE ?1 ESCAPE '\\' ORDER BY ps.rowid DESC LIMIT ?2 OFFSET ?3";
const SEARCH_LIKE_SCOPED: &str = "SELECT ps.session_id, ps.message_id, ps.part_id, m.role, m.time_created, ps.text FROM part_search ps JOIN msg m ON m.id = ps.message_id WHERE ps.session_id = ?2 AND ps.text LIKE ?1 ESCAPE '\\' ORDER BY ps.rowid DESC LIMIT ?3 OFFSET ?4";

pub fn search_parts(
    db: &std::path::Path,
    needle: &str,
    scope: Option<&str>,
    limit: u32,
    offset: u32,
) -> anyhow::Result<(Vec<SearchHit>, bool)> {
    // F7 memo: epoch-exact cache checked BEFORE the reader opens — a hit
    // costs zero db handles (see F7/F8 module for the correctness rule).
    let enabled = search_memo_enabled();
    let key = SearchKey {
        needle: needle.to_string(),
        scope: scope.map(str::to_string),
        limit,
        offset,
    };
    let e0 = write_epoch();
    if enabled {
        let hit = {
            let memo = search_memo();
            let mut m = memo.lock();
            m.get(&key, write_epoch(), db)
        };
        if let Some(v) = hit {
            SEARCH_MEMO_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (h, t) = &*v;
            return Ok((h.clone(), *t));
        }
        SEARCH_MEMO_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let conn = pragma::open_reader(db)?;
    let chars = needle.chars().count();
    let probe = limit as i64 + 1;
    let off = offset as i64;
    // PERF-10X S-A (E1 shootout winner): v10 rowids encode time, so the
    // contract order IS rowid order and the hot path is a pure fts5 walk
    // with LIMIT — it terminates inside the virtual table after LIMIT hits
    // (0.2ms warm / 9.7ms cold measured) instead of the pre-S-A shape that
    // materialized the whole match set (58k rows for "the"), PK-looked-up
    // every row, and sorted full text payloads (5.9-6.3s; EQP: TEMP
    // B-TREE). Scoped searches never materialize the corpus set either: a
    // bounded walk over that session's rowid range with a per-row EXISTS
    // probe (cost ceiling = session size, never corpus size).
    // Single-line SQL: backslash continuations inside strings are the exact
    // bug class check-guards rule 2 exists for -- never split SQL lines.
    let (sql, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = if chars >= 3 {
        // FTS phrase: double quotes are escaped by doubling inside a phrase
        let param: Box<dyn rusqlite::ToSql> =
            Box::new(format!("\"{}\"", needle.replace('"', "\"\"")));
        match scope {
            Some(sc) => (
                SEARCH_MATCH_SCOPED.to_string(),
                vec![
                    param,
                    Box::new(sc.to_string()),
                    Box::new(probe),
                    Box::new(off),
                ],
            ),
            None => (
                SEARCH_MATCH_GLOBAL.to_string(),
                vec![param, Box::new(probe), Box::new(off)],
            ),
        }
    } else {
        let param: Box<dyn rusqlite::ToSql> = Box::new(format!(
            "%{}%",
            needle
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        ));
        match scope {
            Some(sc) => (
                SEARCH_LIKE_SCOPED.to_string(),
                vec![
                    param,
                    Box::new(sc.to_string()),
                    Box::new(probe),
                    Box::new(off),
                ],
            ),
            None => (
                SEARCH_LIKE_GLOBAL.to_string(),
                vec![param, Box::new(probe), Box::new(off)],
            ),
        }
    };
    let mut stmt = conn.prepare_cached(&sql)?;
    let row_slice: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
    let rows: Vec<SearchHit> = stmt
        .query_map(row_slice.as_slice(), row_hit)?
        .filter_map(|r| r.ok())
        .collect();
    let mut hits = rows;
    let truncated = hits.len() as u32 > limit;
    if truncated {
        hits.truncate(limit as usize);
    }
    drop(stmt);
    if enabled && write_epoch() == e0 {
        // raced a write (e1 != e0) => discard rather than store (never stale)
        let memo = search_memo();
        let mut m = memo.lock();
        m.put(key, std::sync::Arc::new((hits.clone(), truncated)), e0, db);
    }
    Ok((hits, truncated))
}

fn row_hit(r: &rusqlite::Row<'_>) -> rusqlite::Result<SearchHit> {
    Ok(SearchHit {
        session_id: r.get(0)?,
        message_id: r.get(1)?,
        part_id: r.get(2)?,
        role: r.get(3)?,
        time: r.get(4)?,
        text: r.get(5)?,
    })
}

/// Snippet: ±80 chars around the first case-insensitive match; all slices
/// clamped to char boundaries (lowercasing can shift byte offsets — never
/// panic, at worst a fuzzy window).
pub fn snippet(text: &str, needle: &str) -> String {
    const WIN: usize = 80;
    let lower = text.to_lowercase();
    let pos = lower.find(&needle.to_lowercase());
    let center = pos.unwrap_or(0);
    let start = floor_boundary(text, center.saturating_sub(WIN));
    let end = ceil_boundary(text, (center + needle.len() + WIN).min(text.len()));
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&text[start..end]);
    if end < text.len() {
        out.push('…');
    }
    out
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
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
        // usage first: session rows accumulate lifetime usage via
        // accumulate_session_usage (upstream applyUsage), NOT finalize.
        accumulate_session_usage(
            &w,
            "ses_f",
            &serde_json::json!({
                "total": 9999, "input": 11, "output": 22, "reasoning": 0,
                "cache": {"write": 0, "read": 33}
            }),
            0.5,
        )
        .unwrap();
        finalize_session_prompt(
            &w,
            "ses_f",
            &PromptStats {
                agent: "build",
                model_json: "{\"id\":\"m\",\"providerID\":\"p\",\"variant\":\"default\"}",
                time_updated: 1234,
            },
        )
        .expect("finalize must not fail (negative control: was silently swallowed)");
        drop(w); // join writer → flushed
        let conn = crate::pragma::open_reader(&db).unwrap();
        let (agent, model, tin, tcache, tout, cost): (String, String, i64, i64, i64, f64) = conn
            .query_row(
                "SELECT agent, model, tokens_input, tokens_cache_read, tokens_output, cost \
                 FROM session WHERE id = 'ses_f'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(agent, "build");
        assert!(
            model.contains("\"providerID\":\"p\""),
            "model json: {model}"
        );
        // the accumulate op wrote the usage; finalize must NOT clobber it
        assert_eq!(tin, 11);
        assert_eq!(tout, 22);
        assert_eq!(tcache, 33);
        assert!((cost - 0.5).abs() < 1e-12, "cost accumulated once: {cost}");
    }

    /// Negative control (AGENTS §1): finalize must NOT overwrite usage once the
    /// accumulate path owns those columns. Pre-fix finalize carried cost/tokens
    /// params and clobbered the row — the planted check would then see 77/88
    /// instead of the accumulated values.
    #[test]
    fn finalize_does_not_overwrite_accumulated_usage() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::writer::db_path(dir.path());
        let w = Writer::spawn(db.clone()).unwrap();
        w.write(vec![WriteOp::Sql {
            sql: "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_g', 'global', '/w', 's', 's', 't', '1', 1, 1)".into(),
            params: vec![],
        }])
        .unwrap();
        accumulate_session_usage(
            &w,
            "ses_g",
            &serde_json::json!({
                "total": 0, "input": 10, "output": 0, "reasoning": 0,
                "cache": {"write": 0, "read": 0}
            }),
            0.1,
        )
        .unwrap();
        accumulate_session_usage(
            &w,
            "ses_g",
            &serde_json::json!({
                "total": 0, "input": 20, "output": 0, "reasoning": 0,
                "cache": {"write": 0, "read": 0}
            }),
            0.2,
        )
        .unwrap();
        finalize_session_prompt(
            &w,
            "ses_g",
            &PromptStats {
                agent: "build",
                model_json: "{\"id\":\"m\",\"providerID\":\"p\"}",
                time_updated: 1,
            },
        )
        .unwrap();
        drop(w);
        let conn = crate::pragma::open_reader(&db).unwrap();
        let (tin, cost): (i64, f64) = conn
            .query_row(
                "SELECT tokens_input, cost FROM session WHERE id = 'ses_g'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(tin, 30, "two steps accumulate; finalize must not reset");
        assert!((cost - 0.3).abs() < 1e-9, "cost adds per step: {cost}");
    }
}

#[cfg(test)]
mod compaction_projection_tests {
    use super::*;
    use serde_json::json;

    fn session_op(id: &str) -> WriteOp {
        WriteOp::Sql {
            sql: "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES (?1, 'global', '/w', 's', 's', 't', '1', 1, 1)".into(),
            params: vec![id.into()],
        }
    }

    #[test]
    fn anchor_part_populates_projection() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::writer::db_path(dir.path());
        let w = Writer::spawn(db.clone()).unwrap();
        w.write(vec![session_op("ses_c1")]).unwrap();
        insert_message(
            &w,
            None,
            "ses_c1",
            &json!({"id":"msg_anchor","sessionID":"ses_c1","role":"user","time":{"created":1}}),
            &[
                json!({"id":"prt_c1","sessionID":"ses_c1","messageID":"msg_anchor",
                     "type":"compaction","auto":true,"overflow":false}),
            ],
        )
        .unwrap();
        drop(w);
        let conn = crate::pragma::open_reader(&db).unwrap();
        let rows = compaction_rows(&conn, "ses_c1").unwrap();
        assert_eq!(rows.len(), 1, "projection row for the anchor part");
        assert_eq!(rows[0].part_id, "prt_c1");
        assert_eq!(rows[0].user_msg_id, "msg_anchor");
        assert!(rows[0].auto, "auto parsed from part JSON");
        assert!(!rows[0].overflow);
        assert_eq!(rows[0].summary_msg_id, None);
    }

    #[test]
    fn update_part_sets_tail_and_summary_links() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::writer::db_path(dir.path());
        let w = Writer::spawn(db.clone()).unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs")).unwrap();
        w.write(vec![session_op("ses_c2")]).unwrap();
        insert_message(
            &w,
            None,
            "ses_c2",
            &json!({"id":"msg_a2","sessionID":"ses_c2","role":"user","time":{"created":1}}),
            &[
                json!({"id":"prt_a2","sessionID":"ses_c2","messageID":"msg_a2",
                     "type":"compaction","auto":true}),
            ],
        )
        .unwrap();
        update_part(
            &w,
            &blobs,
            "ses_c2",
            "msg_a2",
            "prt_a2",
            &json!({"id":"prt_a2","sessionID":"ses_c2","messageID":"msg_a2",
                    "type":"compaction","auto":true,"overflow":false,
                    "tail_start_id":"msg_tail9"}),
        )
        .unwrap();
        // summary assistant links via insert_message info
        insert_message(
            &w,
            None,
            "ses_c2",
            &json!({"id":"msg_sum","sessionID":"ses_c2","role":"assistant",
                    "summary":true,"parentID":"msg_a2","time":{"created":2}}),
            &[],
        )
        .unwrap();
        drop(w);
        let conn = crate::pragma::open_reader(&db).unwrap();
        let rows = compaction_rows(&conn, "ses_c2").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tail_start_id.as_deref(), Some("msg_tail9"));
        assert_eq!(rows[0].summary_msg_id.as_deref(), Some("msg_sum"));
    }

    #[test]
    fn session_delete_cascades_projection() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::writer::db_path(dir.path());
        let w = Writer::spawn(db.clone()).unwrap();
        w.write(vec![session_op("ses_c3")]).unwrap();
        insert_message(
            &w,
            None,
            "ses_c3",
            &json!({"id":"msg_a3","sessionID":"ses_c3","role":"user","time":{"created":1}}),
            &[
                json!({"id":"prt_a3","sessionID":"ses_c3","messageID":"msg_a3",
                     "type":"compaction","auto":true}),
            ],
        )
        .unwrap();
        w.write(vec![WriteOp::Sql {
            sql: "DELETE FROM session WHERE id = 'ses_c3'".into(),
            params: vec![],
        }])
        .unwrap();
        drop(w);
        let conn = crate::pragma::open_reader(&db).unwrap();
        let rows = compaction_rows(&conn, "ses_c3").unwrap();
        assert!(rows.is_empty(), "FK cascade must remove projection rows");
    }

    #[test]
    fn backfill_is_idempotent_and_links_summaries() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::writer::db_path(dir.path());
        // migrate first (Writer::spawn runs the chain), then raw legacy rows
        {
            let w0 = Writer::spawn(db.clone()).unwrap();
            drop(w0);
        }
        {
            let conn = crate::pragma::open_writer(&db).unwrap();
            conn.execute(
                "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_c4', 'global', '/w', 's', 's', 't', '1', 1, 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES \
                 ('msg_leg', 'ses_c4', 'user', 1, 1, '{\"id\":\"msg_leg\",\"role\":\"user\"}')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES \
                 ('msg_leg_s', 'ses_c4', 'assistant', 2, 2, \
                  '{\"id\":\"msg_leg_s\",\"role\":\"assistant\",\"summary\":true,\"parentID\":\"msg_leg\"}')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES \
                 ('prt_leg', 'msg_leg', 'ses_c4', 1, 'compaction', 40, \
                  '{\"id\":\"prt_leg\",\"type\":\"compaction\",\"auto\":true}', NULL)",
                [],
            )
            .unwrap();
        }
        let w = Writer::spawn(db.clone()).unwrap();
        let (n1, l1, _ms) = backfill_compaction(&w, &db).unwrap();
        assert_eq!(n1, 1, "backfilled the legacy anchor");
        assert_eq!(l1, 1, "linked the legacy summary assistant");
        let (n2, l2, _) = backfill_compaction(&w, &db).unwrap();
        assert_eq!((n2, l2), (0, 0), "second backfill is a no-op");
        let conn = crate::pragma::open_reader(&db).unwrap();
        let rows = compaction_rows(&conn, "ses_c4").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].auto);
        assert_eq!(rows[0].summary_msg_id.as_deref(), Some("msg_leg_s"));
    }

    #[test]
    fn part_row_ops_carries_projection_only_for_compaction() {
        let base = PartRow {
            id: "p",
            message_id: "m",
            session_id: "s",
            seq: 1,
            ptype: "text",
            byte_len: 2,
            inline: Some("{}".into()),
            blob_sha: None,
            text: "{}",
        };
        assert_eq!(part_row_ops(&base).len(), 2, "text part: msg_part + search");
        let mut comp = base;
        comp.ptype = "compaction";
        comp.text = r#"{"type":"compaction","auto":true,"overflow":false}"#;
        assert_eq!(
            part_row_ops(&comp).len(),
            3,
            "compaction part adds projection"
        );
    }

    /// Crash-window self-heal (importer bulk-path fix, 2026-10-11): the importer
    /// DROPS `part_search_ai`, bulk-rebuilds the FTS, then recreates the
    /// trigger. A kill mid-flight leaves (a) the trigger missing and/or
    /// (b) `docsize < part_search count`. `backfill_part_search` must repair
    /// both halves; the pre-state assertions prove the planted state is
    /// genuinely broken so the repair assertions aren't vacuous.
    #[test]
    fn backfill_repairs_interrupted_import_crash_window() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::writer::db_path(dir.path());
        let w = Writer::spawn(db.clone()).unwrap();
        w.write(vec![session_op("ses_heal")]).unwrap();
        insert_message(
            &w,
            None,
            "ses_heal",
            &json!({"id":"msg_heal","sessionID":"ses_heal","role":"user","time":{"created":1}}),
            &[
                json!({"id":"prt_heal","sessionID":"ses_heal","messageID":"msg_heal",
                     "type":"text","text":"healing needle"}),
            ],
        )
        .unwrap();
        drop(w);

        // Simulate the kill exactly as the bulk path leaves it: index wiped,
        // trigger gone.
        {
            let conn = crate::pragma::open_writer(&db).unwrap();
            conn.execute_batch(
                "INSERT INTO part_search_fts(part_search_fts) VALUES('delete-all');
                 DROP TRIGGER part_search_ai;",
            )
            .unwrap();
        }
        {
            let c = crate::pragma::open_reader(&db).unwrap();
            let docs: i64 = c
                .query_row("SELECT count(*) FROM part_search_fts_docsize", [], |r| {
                    r.get(0)
                })
                .unwrap();
            let trg: i64 = c
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name='part_search_ai'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let rows: i64 = c
                .query_row("SELECT count(*) FROM part_search", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                (docs, trg, rows),
                (0, 0, 1),
                "planted crash window must actually be broken"
            );
        }

        let w = Writer::spawn(db.clone()).unwrap();
        let r = backfill_part_search(&w, &db).unwrap();
        assert_eq!((r.0, r.1), (0, 0), "repair path reports no new indexing");
        {
            let c = crate::pragma::open_reader(&db).unwrap();
            let docs: i64 = c
                .query_row("SELECT count(*) FROM part_search_fts_docsize", [], |r| {
                    r.get(0)
                })
                .unwrap();
            let trg: i64 = c
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name='part_search_ai'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let hits: i64 = c
                .query_row(
                    "SELECT count(*) FROM part_search_fts WHERE part_search_fts MATCH '\"needle\"'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(trg, 1, "trigger recreated");
            assert_eq!(docs, 1, "index rebuilt (docsize == count)");
            assert_eq!(hits, 1, "repaired index is searchable");
        }

        // Second half of the window: trigger missing but index INTACT — the
        // repair must recreate the trigger without needing a rebuild, and a
        // subsequent store-path insert must be indexed by the new trigger.
        {
            let conn = crate::pragma::open_writer(&db).unwrap();
            conn.execute_batch("DROP TRIGGER part_search_ai;").unwrap();
        }
        let r2 = backfill_part_search(&w, &db).unwrap();
        assert_eq!((r2.0, r2.1), (0, 0));
        insert_message(
            &w,
            None,
            "ses_heal",
            &json!({"id":"msg_heal2","sessionID":"ses_heal","role":"user","time":{"created":2}}),
            &[
                json!({"id":"prt_heal2","sessionID":"ses_heal","messageID":"msg_heal2",
                     "type":"text","text":"second needle"}),
            ],
        )
        .unwrap();
        let c = crate::pragma::open_reader(&db).unwrap();
        let hits: i64 = c
            .query_row(
                "SELECT count(*) FROM part_search_fts WHERE part_search_fts MATCH '\"needle\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 2, "recreated trigger indexes new writes");
    }
}

#[cfg(test)]
mod f3_chunk {
    use super::*;
    use crate::writer::db_path;

    /// Chunk-boundary BVA (TESTING §1: EP/BVA on chunk boundaries): 70
    /// messages cross PART_CHUNK=64 — parts on msgs 1/64/65/70 must all
    /// appear, in message order, with no duplicates or drops at either
    /// edge of the IN batch.
    #[test]
    fn part_batches_cross_chunk_boundary_intact() {
        let dir = tempfile::tempdir().unwrap();
        let db = db_path(dir.path());
        let w = Writer::spawn(db.clone()).unwrap();
        let mut ops = vec![WriteOp::Sql {
            sql: "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_c', 'global', '/w', 's', 's', 't', '1', 1, 1)".into(),
            params: vec![],
        }];
        for i in 1..=70i64 {
            ops.push(WriteOp::Sql {
                sql: format!(
                    "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('m{i}', 'ses_c', 'user', {i}, {i}, '{{}}')"
                ),
                params: vec![],
            });
            ops.push(WriteOp::Sql {
                sql: format!(
                    "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('p{i}', 'm{i}', 'ses_c', 1, 'text', 4, '{{\"type\":\"text\",\"text\":\"part{i}\"}}', NULL)"
                ),
                params: vec![],
            });
        }
        w.write(ops).unwrap();
        let mut frames: Vec<String> = Vec::new();
        for_each_message_json(&db, "ses_c", MessageWalk::Seq { limit: None }, |chunk| {
            frames.push(chunk);
            Ok(())
        })
        .unwrap();
        assert_eq!(frames.len(), 70, "every message visited once");
        for i in [1, 63, 64, 65, 70] {
            let f = &frames[i - 1];
            assert!(
                f.contains(&format!("\"part{i}\"")),
                "frame {i} must carry its part: {f}"
            );
            if i > 1 {
                assert!(
                    !f.contains(&format!("\"part{}\"", i - 1)) || i == 65,
                    "frame {i} must not carry the previous part: {f}"
                );
            }
        }
        // exact order: seq asc inside each message (single part each) and
        // time_created asc across frames (frame 1 = m1 ... frame 70 = m70)
        assert!(frames[64].contains("\"part65\""), "post-boundary frame");
        assert!(frames[63].contains("\"part64\""), "boundary frame");
    }
}

#[cfg(test)]
mod memo_switch_table {
    use super::memo_enabled;

    /// Kill-switch truth table (pure — no env, no globals).
    #[test]
    fn memo_enabled_table() {
        assert!(memo_enabled(None, None), "unset = on");
        assert!(memo_enabled(None, Some("1")), "explicit 1 = on");
        assert!(memo_enabled(None, Some("auto")), "any non-0 = on");
        assert!(!memo_enabled(None, Some("0")), "0 = off");
        assert!(
            memo_enabled(Some(true), Some("0")),
            "override wins over env"
        );
        assert!(!memo_enabled(Some(false), None), "override wins over unset");
    }
}
