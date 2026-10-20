//! Single-writer task: all writes batched through one channel, transactions ≤50 ms.
//! Connections never leave this task (no checkout across `.await` by construction).

use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
/// A unit of work executed inside the writer transaction.
/// Handlers capture plain data — no Connection escapes this enum's scope.
pub enum WriteOp {
    /// Raw SQL batch with params already bound as (sql, params-as-json) for simplicity
    /// in M1; typed ops replace these as the domain grows.
    Sql {
        sql: String,
        params: Vec<serde_json::Value>,
    },
    /// Same as `Sql`, but captures one value from the statement's first
    /// returned row. Used for allocation that MUST be atomic with the write:
    /// `INSERT ... RETURNING seq`. Doing this with a separate reader after the
    /// commit is a race (two emitters can both read the same MAX — proven by
    /// `tests/event_seq.rs`, 115 duplicates in 200 concurrent emits).
    SqlReturning {
        sql: String,
        params: Vec<serde_json::Value>,
        capture: Capture,
    },
    /// Register a blob object row (sha, len, chunks) after BlobStore::put completed.
    BlobPut {
        sha: String,
        byte_len: u64,
        chunk_cnt: u32,
    },
}

/// What `WriteOp::SqlReturning` captures from the first returned row.
#[derive(Clone, Copy, Debug)]
pub enum Capture {
    /// A single integer column (`RETURNING seq`).
    FirstColumnI64,
}

/// Result of one committed batch: rows affected, plus captured values in the
/// order their ops appeared.
#[derive(Debug, Default)]
pub struct BatchAck {
    pub affected: usize,
    pub captured: Vec<i64>,
}

struct Batch {
    ops: Vec<WriteOp>,
    ack: mpsc::Sender<Result<BatchAck>>,
}

/// Best-effort pre-migration snapshot (M1): when the on-disk `user_version`
/// is BELOW the binary's `SCHEMA_VERSION`, copy the DB into
/// `<data>/.safety/ocserve-pre-v<from>.db` before the version advances, so a
/// rollback to the previous tag can restore it (an older binary refuses to
/// boot a newer-versioned DB). No-op when already current (the common boot) or
/// when the version is UNKNOWN/newer (that path bails in migrate anyway).
/// Non-fatal: a failed backup warns and proceeds (the backup is a convenience,
/// not a boot gate — the same fail-soft rule as post_maintenance).
fn backup_before_migration(conn: &Connection, path: &Path) -> Result<()> {
    let from: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    if from == 0 || from >= crate::schema::SCHEMA_VERSION {
        return Ok(()); // fresh DB (no data to lose) or already current
    }
    let dir = match path.parent() {
        Some(d) => d.join(".safety"),
        None => return Ok(()),
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("pre-migration backup: mkdir {}: {e:#}", dir.display());
        return Ok(());
    }
    let dest = dir.join(format!("ocserve-pre-v{from}.db"));
    if dest.exists() {
        return Ok(()); // an earlier backup for this version already exists
    }
    match conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()]) {
        Ok(_) => tracing::info!(
            "pre-migration backup: v{from} -> {} (restore with this file to roll back)",
            dest.display()
        ),
        Err(e) => tracing::warn!("pre-migration backup failed (continuing): {e:#}"),
    }
    Ok(())
}

pub struct Writer {
    tx: Option<mpsc::Sender<Batch>>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// in-flight batches (send +1, ack -1) — writer_queue_depth metric source
    inflight: std::sync::Arc<std::sync::atomic::AtomicI64>,
    /// DB path this writer owns (event retention's over-cap gate needs a
    /// reader on the same file).
    path: PathBuf,
}

impl Writer {
    /// Spawn the writer thread on `path`. Caller owns the commit cadence:
    /// ops are drained until either the channel dries or `max_wait` elapses.
    pub fn spawn(path: PathBuf) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Batch>();
        // Open + migrate SYNCHRONOUSLY (fail-fast at boot; the DB must exist
        // and be migrated before any reader can race us — M2 500 finding).
        let conn = crate::pragma::open_writer(&path)?;
        // M1 (reliability review): a schema-ADVANCING migration is one-way —
        // once user_version moves up, an older binary refuses to boot
        // (`bail!` on a newer version), so a rollback would be bricked. Take a
        // best-effort `VACUUM INTO` snapshot into a `.safety/` sibling BEFORE
        // the version advances, so the previous tag can be restored. Skipped
        // when already current (the common boot) and non-fatal on failure.
        backup_before_migration(&conn, &path)?;
        crate::schema::migrate(&conn)?;
        let handle = std::thread::Builder::new()
            .name("ocserve-writer".into())
            .spawn(move || run_loop(&conn, rx))?;
        Ok(Self {
            tx: Some(tx),
            handle: Some(handle),
            inflight: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
            path,
        })
    }

    /// Path of the DB this writer owns.
    pub fn db_path_ref(&self) -> &std::path::Path {
        &self.path
    }

    /// In-flight batch count (SRE writer_queue_depth gauge source).
    pub fn queue_depth(&self) -> i64 {
        self.inflight.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Submit ops; blocks until the batch commits. Returns rows affected (sum).
    pub fn write(&self, ops: Vec<WriteOp>) -> Result<usize> {
        self.write_capture(ops).map(|ack| ack.affected)
    }

    /// Submit ops and return the full ack (rows affected + captured values).
    /// Blocks until the batch commits — the transaction is already durable by
    /// the time any captured value is returned, so it cannot race another
    /// writer batch.
    pub fn write_capture(&self, ops: Vec<WriteOp>) -> Result<BatchAck> {
        let (ack_tx, ack_rx) = mpsc::channel();
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("writer closed"))?;
        self.inflight
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tx.send(Batch { ops, ack: ack_tx })
            .map_err(|_| anyhow::anyhow!("writer thread gone"))?;
        let res = ack_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("writer dropped ack"))?;
        self.inflight
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        res
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Close the channel BEFORE joining, or the writer thread never exits.
        drop(self.tx.take());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn bind(conn: &Connection, sql: &str, params: &[serde_json::Value]) -> Result<usize> {
    // Cutting-edge statement path (PERF-10X stmt audit): prepare_cached
    // prepares ONCE with SQLITE_PREPARE_PERSISTENT (rusqlite's documented
    // behaviour) and reuses the VDBE across batches — every WriteOp::Sql
    // funnels here, so op-SQL compile cost drops to LRU hits.
    // SQLite 3.53.2 schema-state bug (found 2026-10-11): after ANOTHER
    // connection on the same file recreates a DDL object (the FTS
    // crash-window self-heal recreates part_search_ai), this connection's
    // next fresh prepare can fail with a spurious "no such table" while
    // sqlite_master clearly contains it. The failure is one-shot: a second
    // prepare of the SAME sql on the SAME connection succeeds. Flush the
    // statement cache and retry exactly once; a real missing table fails
    // both times and still propagates. (Proven: minimal repro
    // `expect_no_error_with_fixed_bind` was red without this, green with.)
    let mut stmt = match conn.prepare_cached(sql) {
        Ok(s) => s,
        Err(first) => {
            conn.flush_prepared_statement_cache();
            conn.prepare_cached(sql).inspect(|_| {
                tracing::debug!("prepare_cached: one-shot schema glitch recovered ({first})");
            })?
        }
    };
    let boxed: Vec<Box<dyn rusqlite::ToSql>> = params
        .iter()
        .map(|v| match v {
            serde_json::Value::Null => Box::new(rusqlite::types::Value::Null) as _,
            serde_json::Value::Bool(b) => Box::new(*b as i64) as _,
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Box::new(i) as _
                } else if let Some(f) = n.as_f64() {
                    Box::new(f) as _
                } else {
                    Box::new(n.to_string()) as _
                }
            }
            serde_json::Value::String(s) => Box::new(s.clone()) as _,
            other => Box::new(other.to_string()) as _,
        })
        .collect();
    let refs: Vec<&dyn rusqlite::ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
    Ok(stmt.execute(refs.as_slice())?)
}

fn run_loop(conn: &Connection, rx: mpsc::Receiver<Batch>) {
    loop {
        let first = match rx.recv() {
            Ok(b) => b,
            Err(_) => return, // sender dropped: shutdown
        };
        let deadline = Instant::now() + Duration::from_millis(50);
        let mut batch = vec![first];
        // coalesce for up to 50 ms (write amplification ↓, latency bounded)
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(b) => batch.push(b),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        let total_ops: usize = batch.iter().map(|b| b.ops.len()).sum();
        let mut affected = 0usize;
        let mut captured: Vec<i64> = Vec::new();
        // Per-caller results. A coalesced transaction commits once, but each
        // caller must see ITS OWN rows-affected and captured values: the
        // previous code handed every ack the coalesced totals, so two
        // concurrent `INSERT OR IGNORE` callers both observed n=2 and each
        // concluded "someone else inserted" (inbox::admit reads `n == 1`).
        let mut per_batch_affected: Vec<usize> = vec![0; batch.len()];
        let mut per_batch_captured: Vec<Vec<i64>> = vec![Vec::new(); batch.len()];
        // Track failure as a formatted string: anyhow::Error is not Clone, and each
        // batch ack needs its own Result.
        let mut failure: Option<String> = None;
        // one transaction for the whole coalesced batch
        if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
            failure = Some(format!("{e:#}"));
        } else {
            for (i, b) in batch.iter().enumerate() {
                if failure.is_some() {
                    break;
                }
                let affected_before = affected;
                let captured_before = captured.len();
                for op in &b.ops {
                    if failure.is_some() {
                        break;
                    }
                    if let Err(e) = exec_op(conn, op, &mut affected, &mut captured) {
                        failure = Some(format!("{e:#}"));
                    }
                }
                per_batch_affected[i] = affected - affected_before;
                per_batch_captured[i] = captured[captured_before..].to_vec();
            }
            match &failure {
                None => {
                    if let Err(e) = conn.execute_batch("COMMIT") {
                        failure = Some(format!("commit: {e:#}"));
                        let _ = conn.execute_batch("ROLLBACK");
                    } else {
                        // F7/F8: committed => epoch bump + eager memo clear
                        // (exact invalidation for the single-writer caches)
                        crate::bump_write_epoch();
                    }
                }
                Some(_) => {
                    let _ = conn.execute_batch("ROLLBACK");
                }
            }
        }
        let _ = total_ops;
        // Each caller gets its own slice of the coalesced commit. On failure
        // every ack reports the error (the transaction rolled back as a unit).
        for (i, b) in batch.into_iter().enumerate() {
            let outcome = match &failure {
                None => Ok(BatchAck {
                    affected: per_batch_affected[i],
                    captured: std::mem::take(&mut per_batch_captured[i]),
                }),
                Some(msg) => Err(anyhow::anyhow!("{msg}")),
            };
            let _ = b.ack.send(outcome);
        }
    }
}

/// Bind params and run a statement that returns a row, capturing one column.
/// Shares the schema-glitch recovery of `bind` (see its comment): a foreign
/// DDL on another connection can poison the next fresh prepare once.
fn bind_returning(
    conn: &Connection,
    sql: &str,
    params: &[serde_json::Value],
    capture: Capture,
) -> Result<i64> {
    let mut stmt = match conn.prepare_cached(sql) {
        Ok(s) => s,
        Err(first) => {
            conn.flush_prepared_statement_cache();
            conn.prepare_cached(sql).inspect(|_| {
                tracing::debug!("prepare_cached(returning): recovered ({first})");
            })?
        }
    };
    let boxed: Vec<Box<dyn rusqlite::ToSql>> = params
        .iter()
        .map(|v| match v {
            serde_json::Value::Null => Box::new(rusqlite::types::Value::Null) as _,
            serde_json::Value::Bool(b) => Box::new(*b as i64) as _,
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Box::new(i) as _
                } else if let Some(f) = n.as_f64() {
                    Box::new(f) as _
                } else {
                    Box::new(n.to_string()) as _
                }
            }
            serde_json::Value::String(s) => Box::new(s.clone()) as _,
            other => Box::new(other.to_string()) as _,
        })
        .collect();
    let refs: Vec<&dyn rusqlite::ToSql> = boxed.iter().map(|b| b.as_ref()).collect();
    let value = match capture {
        Capture::FirstColumnI64 => stmt.query_row(refs.as_slice(), |r| r.get::<_, i64>(0))?,
    };
    Ok(value)
}

fn exec_op(
    conn: &Connection,
    op: &WriteOp,
    affected: &mut usize,
    captured: &mut Vec<i64>,
) -> Result<()> {
    match op {
        WriteOp::Sql { sql, params } => {
            *affected += bind(conn, sql, params)?;
            Ok(())
        }
        WriteOp::SqlReturning {
            sql,
            params,
            capture,
        } => {
            let value = bind_returning(conn, sql, params, *capture)?;
            captured.push(value);
            *affected += 1;
            Ok(())
        }
        WriteOp::BlobPut {
            sha,
            byte_len,
            chunk_cnt,
        } => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let sql =
                "INSERT OR REPLACE INTO blob_object (sha, byte_len, chunk_cnt, codec, created_at)
                 VALUES (?1, ?2, ?3, 'zstd', ?4)";
            let run = |c: &Connection| {
                c.execute(
                    sql,
                    rusqlite::params![sha, *byte_len as i64, *chunk_cnt, now],
                )
            };
            // same one-shot schema-glitch recovery as bind() (see comment there)
            match run(conn) {
                Ok(n) => *affected += n,
                Err(_) => {
                    conn.flush_prepared_statement_cache();
                    *affected += run(conn)?;
                }
            }
            Ok(())
        }
    }
}

/// Execute ops on a CALLER-owned connection (the importer runs inside its
/// own explicit transaction on the dst connection — same param conversion
/// as the writer thread). Keeps msg_part writes funnelled through the
/// part_* op builders (check-guards rule 4).
pub fn apply_ops(conn: &Connection, ops: &[WriteOp]) -> Result<usize> {
    let mut affected = 0;
    let mut captured = Vec::new();
    for op in ops {
        exec_op(conn, op, &mut affected, &mut captured)?;
    }
    if !ops.is_empty() {
        // F7/F8: caller-owned batches (importer/fork/tests) invalidate too
        crate::bump_write_epoch();
    }
    Ok(affected)
}

/// Path helpers shared with the CLI.
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ocserve.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_commits_and_migrates() {
        let d = tempfile::tempdir().unwrap();
        let w = Writer::spawn(db_path(d.path())).unwrap();
        let n = w
            .write(vec![WriteOp::Sql {
                sql: "INSERT INTO session (id, time_created, time_updated) VALUES (?1, ?2, ?3)"
                    .into(),
                params: vec!["ses_t".into(), 1.into(), 2.into()],
            }])
            .unwrap();
        assert_eq!(n, 1);
        let r = crate::pragma::open_reader(&db_path(d.path())).unwrap();
        let id: String = r
            .query_row("SELECT id FROM session", [], |row| row.get(0))
            .unwrap();
        assert_eq!(id, "ses_t");
    }

    #[test]
    fn failed_batch_rolls_back_whole_batch() {
        let d = tempfile::tempdir().unwrap();
        let w = Writer::spawn(db_path(d.path())).unwrap();
        let res = w.write(vec![
            WriteOp::Sql {
                sql: "INSERT INTO session (id, time_created, time_updated) VALUES (?1, ?2, ?3)"
                    .into(),
                params: vec!["ok".into(), 1.into(), 1.into()],
            },
            WriteOp::Sql {
                sql: "INSERT INTO nosuchtable VALUES (1)".into(),
                params: vec![],
            },
        ]);
        assert!(res.is_err());
        let r = crate::pragma::open_reader(&db_path(d.path())).unwrap();
        let cnt: i64 = r
            .query_row("SELECT count(*) FROM session", [], |x| x.get(0))
            .unwrap();
        assert_eq!(cnt, 0, "partial batch must not commit");
    }

    #[test]
    fn blob_put_registers_object() {
        let d = tempfile::tempdir().unwrap();
        let w = Writer::spawn(db_path(d.path())).unwrap();
        w.write(vec![WriteOp::BlobPut {
            sha: "aa".repeat(32),
            byte_len: 3,
            chunk_cnt: 1,
        }])
        .unwrap();
        let r = crate::pragma::open_reader(&db_path(d.path())).unwrap();
        let n: i64 = r
            .query_row("SELECT count(*) FROM blob_object", [], |x| x.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
