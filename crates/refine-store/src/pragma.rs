//! Connection open routines: the ONLY place PRAGMAs are issued (STORAGE.md §2).
//!
//! Rules enforced here:
//! - version gate: bundled SQLite >= 3.51.3 (WAL-reset fix), refuse otherwise
//! - creation profile runs before any table exists (page_size, auto_vacuum)
//! - role-scoped cache sizes, mmap off, temp_store=FILE
//! - never re-issue journal_mode on a live handle

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

/// Minimum SQLite version with the WAL-reset corruption fix (changes 3.51.3, 2026-03-13).
pub const MIN_SQLITE_VERSION: &str = "3.51.3";

/// Aggregate page-cache budget across ALL connections (MEMORY.md §1: 32 MB).
pub const WRITER_CACHE_KB: i64 = -16_384; // 16 MB
pub const READER_CACHE_KB: i64 = -4_096; // 4 MB each, pool of 4 = 16 MB

fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    // strip any suffix like "3.53.0-beta"
    let patch = it
        .next()
        .map(|p| p.split(['-', '+']).next().unwrap_or(p))
        .and_then(|p| p.parse().ok())?;
    Some((major, minor, patch))
}

/// Boot-time version gate. Fails fast with an actionable message (SRE.md §1).
pub fn assert_version_ok() -> Result<()> {
    let v = rusqlite::version();
    let got = parse_version(v).with_context(|| format!("unparseable sqlite version {v}"))?;
    let want = parse_version(MIN_SQLITE_VERSION).expect("const parses");
    if got < want {
        bail!(
            "SQLite {v} < required {MIN_SQLITE_VERSION} (WAL-reset corruption fix). \
             rebuild with rusqlite `bundled` feature; do not link a system libsqlite3."
        );
    }
    Ok(())
}

fn apply_common(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "mmap_size", 0)
        .context("mmap_size")?;
    // 1 = FILE: bounded. The old value was 2 (= MEMORY) with a comment
    // claiming FILE — an unbounded temp ceiling on every connection.
    // PERF-10X temp_store decision: S-A removed the large sorts (E1:
    // search = fts rowid walk, no temp b-tree), remaining temp = small
    // maintenance sorts (event-window prunes) — disk-bound is the right
    // default again.
    conn.pragma_update(None, "temp_store", 1)
        .context("temp_store")?;
    conn.pragma_update(None, "threads", 0).context("threads")?;
    conn.pragma_update(None, "cell_size_check", 1)
        .context("cell_size_check")?;
    conn.pragma_update(None, "trusted_schema", 0)
        .context("trusted_schema")?;
    conn.pragma_update(None, "foreign_keys", 1)
        .context("foreign_keys")?;
    conn.pragma_update(None, "busy_timeout", 5000)
        .context("busy_timeout")?;
    // Statement cache sized explicitly (default is 16): hot read paths use
    // 15+ distinct static statements across routes; 32 heads off LRU thrash
    // and is still <1 MB of VDBE. prepare_cached = SQLITE_PREPARE_PERSISTENT
    // (rusqlite 0.40) — the modern reuse path, audited PERF-10X stmt pass.
    conn.set_prepared_statement_cache_capacity(32);
    Ok(())
}

/// Create a brand-new database with the creation profile (STORAGE.md §2).
/// Refuses if the file already exists with tables (creation profile must precede schema).
pub fn create_new(path: &std::path::Path) -> Result<Connection> {
    assert_version_ok()?;
    let conn = Connection::open(path).context("open for create")?;
    conn.pragma_update(None, "page_size", 4096)
        .context("page_size")?;
    // auto_vacuum MUST precede any CREATE TABLE (STORAGE.md §2)
    conn.pragma_update(None, "auto_vacuum", 1)
        .context("auto_vacuum")?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("journal_mode")?;
    conn.pragma_update(None, "synchronous", 1)
        .context("synchronous")?; // NORMAL
    conn.pragma_update(None, "wal_autocheckpoint", 1000)
        .context("wal_autocheckpoint")?;
    conn.pragma_update(None, "journal_size_limit", 67_108_864)
        .context("journal_size_limit")?;
    // optimize intentionally NOT run here: tables don't exist yet (the old
    // call was a perpetual no-op). schema::migrate runs it at the end of
    // every writer spawn, post-DDL — the documented long-lived pattern.
    apply_common(&conn)?;
    conn.pragma_update(None, "cache_size", WRITER_CACHE_KB)
        .context("writer cache_size")?;
    Ok(conn)
}

/// Open the writer connection: creation profile if new, else reopen profile.
pub fn open_writer(path: &std::path::Path) -> Result<Connection> {
    assert_version_ok()?;
    let fresh = !path.exists();
    let conn = if fresh {
        create_new(path)?
    } else {
        let conn = Connection::open(path).context("open writer")?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("journal_mode")?;
        conn.pragma_update(None, "synchronous", 1)
            .context("synchronous")?;
        conn.pragma_update(None, "wal_autocheckpoint", 1000)
            .context("wal_autocheckpoint")?;
        conn.pragma_update(None, "journal_size_limit", 67_108_864)
            .context("journal_size_limit")?;
        apply_common(&conn)?;
        conn
    };
    conn.pragma_update(None, "cache_size", WRITER_CACHE_KB)
        .context("writer cache_size")?;
    Ok(conn)
}

/// Open a read-only reader connection (STORAGE.md §2): ro URI + query_only + small cache.
/// K-EFFICIENCY: reader-open accounting (core snapshots this per prompt).
pub static READER_OPENS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn reader_opens() -> u64 {
    READER_OPENS.load(std::sync::atomic::Ordering::Relaxed)
}

/// K-EFFICIENCY (phase 1, bytehound group #1): a reader connection is
/// PARKED in thread-local storage on drop and checked out again for the
/// same path — kills the per-query fresh-open + page-cache reallocation
/// churn (profile: pcache1Alloc via get_messages was the largest byte
/// group). Reentrancy is safe: an open while parked is checked out falls
/// back to a fresh transient connection (dropped, never parked).
pub struct Reader {
    conn: Option<Connection>,
    db: std::path::PathBuf,
}

impl std::ops::Deref for Reader {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("Reader.conn taken exactly once")
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        let db = std::mem::take(&mut self.db);
        TLS_SLOT.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some((db, conn));
            }
            // slot already holds another path's conn → drop this one
        });
    }
}

std::thread_local! {
    static TLS_SLOT: std::cell::RefCell<Option<(std::path::PathBuf, Connection)>> =
        const { std::cell::RefCell::new(None) };
}

pub fn open_reader(path: &std::path::Path) -> Result<Reader> {
    let parked = TLS_SLOT.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.as_ref() {
            Some((db, _)) if db == path => slot.take().map(|(_, c)| c),
            _ => None,
        }
    });
    if let Some(conn) = parked {
        return Ok(Reader {
            conn: Some(conn),
            db: path.to_path_buf(),
        });
    }
    READER_OPENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    assert_version_ok()?;
    let uri = format!(
        "file:{}?mode=ro",
        path.to_str().context("path utf-8")?.replace('?', "%3f")
    );
    let conn = Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_URI | rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .context("open reader")?;
    apply_common(&conn)?;
    conn.pragma_update(None, "query_only", 1)
        .context("query_only")?;
    conn.pragma_update(None, "cache_size", READER_CACHE_KB)
        .context("reader cache_size")?;
    Ok(Reader {
        conn: Some(conn),
        db: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // K-EFFICIENCY reader accounting moved to tests/reader_accounting.rs:
    // exact deltas on the process-global READER_OPENS counter are unsound
    // under the in-src parallel test runner (flaked 2026-10-05 — TESTING §1.6).

    #[test]
    fn version_gate_passes_on_bundled() {
        // M0 decisive experiment: bundled must be >= 3.51.3 (AGENTS/PLAN §13).
        assert_version_ok().expect("bundled SQLite must satisfy the gate");
        let v = parse_version(rusqlite::version()).unwrap();
        assert!(v >= (3, 51, 3), "got {:?}", v);
    }

    #[test]
    fn version_parse_handles_suffixes() {
        assert_eq!(parse_version("3.53.0"), Some((3, 53, 0)));
        assert_eq!(parse_version("3.51.3-beta1"), Some((3, 51, 3)));
        assert_eq!(parse_version("3.9.0"), Some((3, 9, 0)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn creation_profile_applies_before_schema() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let conn = create_new(&p).unwrap();
        let pv: i64 = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pv, 4096);
        let av: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap();
        assert_eq!(av, 1, "INCREMENTAL must be set before tables");
        let jm: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jm.to_lowercase(), "wal");
        let ms: i64 = conn
            .query_row("PRAGMA mmap_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ms, 0, "mmap off per CIDR 2022");
        let cs: i64 = conn
            .query_row("PRAGMA cache_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cs, WRITER_CACHE_KB);
        conn.execute_batch("CREATE TABLE x(a INTEGER)").unwrap();
    }

    #[test]
    fn writer_reopen_keeps_creation_profile() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        drop(create_new(&p).unwrap());
        let conn = open_writer(&p).unwrap();
        let av: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap();
        assert_eq!(av, 1);
        let cs: i64 = conn
            .query_row("PRAGMA cache_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cs, WRITER_CACHE_KB);
    }

    #[test]
    fn reader_is_query_only_with_small_cache() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        drop(create_new(&p).unwrap());
        let r = open_reader(&p).unwrap();
        let cs: i64 = r.query_row("PRAGMA cache_size", [], |x| x.get(0)).unwrap();
        assert_eq!(cs, READER_CACHE_KB);
        let err = r.execute_batch("CREATE TABLE z(a)");
        assert!(err.is_err(), "reader must reject writes");
    }
}
