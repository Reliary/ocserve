//! Single-writer task: all writes batched through one channel, transactions ≤50 ms.
//! Connections never leave this task (no checkout across `.await` by construction).

use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A unit of work executed inside the writer transaction.
/// Handlers capture plain data — no Connection escapes this enum's scope.
pub enum WriteOp {
    /// Raw SQL batch with params already bound as (sql, params-as-json) for simplicity
    /// in M1; typed ops replace these as the domain grows.
    Sql {
        sql: String,
        params: Vec<serde_json::Value>,
    },
    /// Register a blob object row (sha, len, chunks) after BlobStore::put completed.
    BlobPut {
        sha: String,
        byte_len: u64,
        chunk_cnt: u32,
    },
}

struct Batch {
    ops: Vec<WriteOp>,
    ack: mpsc::Sender<Result<usize>>,
}

pub struct Writer {
    tx: Option<mpsc::Sender<Batch>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Writer {
    /// Spawn the writer thread on `path`. Caller owns the commit cadence:
    /// ops are drained until either the channel dries or `max_wait` elapses.
    pub fn spawn(path: PathBuf) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Batch>();
        let handle = std::thread::Builder::new()
            .name("refine-writer".into())
            .spawn(move || {
                let conn = match crate::pragma::open_writer(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        // Fail fast at boot; the sender will see closed-channel errors.
                        tracing::error!("writer open failed: {e:#}");
                        return;
                    }
                };
                if let Err(e) = crate::schema::migrate(&conn) {
                    tracing::error!("schema migrate failed: {e:#}");
                    return;
                }
                run_loop(&conn, rx);
            })?;
        Ok(Self {
            tx: Some(tx),
            handle: Some(handle),
        })
    }

    /// Submit ops; blocks until the batch commits. Returns rows affected (sum).
    pub fn write(&self, ops: Vec<WriteOp>) -> Result<usize> {
        let (ack_tx, ack_rx) = mpsc::channel();
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("writer closed"))?;
        tx.send(Batch { ops, ack: ack_tx })
            .map_err(|_| anyhow::anyhow!("writer thread gone"))?;
        ack_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("writer dropped ack"))?
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
    let mut stmt = conn.prepare(sql)?;
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
        // Track failure as a formatted string: anyhow::Error is not Clone, and each
        // batch ack needs its own Result.
        let mut failure: Option<String> = None;
        // one transaction for the whole coalesced batch
        if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
            failure = Some(format!("{e:#}"));
        } else {
            for b in &batch {
                if failure.is_some() {
                    break;
                }
                for op in &b.ops {
                    if failure.is_some() {
                        break;
                    }
                    if let Err(e) = exec_op(conn, op, &mut affected) {
                        failure = Some(format!("{e:#}"));
                    }
                }
            }
            match &failure {
                None => {
                    if let Err(e) = conn.execute_batch("COMMIT") {
                        failure = Some(format!("commit: {e:#}"));
                        let _ = conn.execute_batch("ROLLBACK");
                    }
                }
                Some(_) => {
                    let _ = conn.execute_batch("ROLLBACK");
                }
            }
        }
        let _ = total_ops;
        for b in batch {
            let outcome = match &failure {
                None => Ok(affected),
                Some(msg) => Err(anyhow::anyhow!("{msg}")),
            };
            let _ = b.ack.send(outcome);
        }
    }
}

fn exec_op(conn: &Connection, op: &WriteOp, affected: &mut usize) -> Result<()> {
    match op {
        WriteOp::Sql { sql, params } => {
            *affected += bind(conn, sql, params)?;
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
            *affected += conn.execute(
                "INSERT OR REPLACE INTO blob_object (sha, byte_len, chunk_cnt, codec, created_at)
                 VALUES (?1, ?2, ?3, 'zstd', ?4)",
                rusqlite::params![sha, *byte_len as i64, *chunk_cnt, now],
            )?;
            Ok(())
        }
    }
}

/// Path helpers shared with the CLI.
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("refine.db")
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
