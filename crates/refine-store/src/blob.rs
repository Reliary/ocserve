//! Chunked zstd blob store with the crash protocol (STORAGE.md §4):
//! write chunk file → fsync → rename → then DB commit in the writer batch.
//! Crash between file write and DB commit leaves orphans (swept by `gc`);
//! crash before rename leaves no visible file (safe).
//!
//! Content-addressed: sha256 of the FULL logical object names the object;
//! chunks are sha256(ordinal || data) files under chunks/<sha[0..2]>/<full-sha>/<ord>.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Chunk target size (1 MiB, MEMORY/PLAN §5). Last chunk may be smaller.
pub const CHUNK_BYTES: usize = 1_048_576;
/// RFC 8878 block ceiling guidance: zstd frames inside a chunk are ≤128 KiB blocks
/// (frame size is zstd's internal business; we set the frame level only).
pub const ZSTD_LEVEL: i32 = 3;
/// Single-allocation cap for any payload window (MEMORY.md §6).
pub const MAX_ALLOC: usize = 8 * 1024 * 1024;

// ---- PERF-10X B1: process-global assembled-blob cache ----
//
// Trigger (Phase-I probe): deep-page reads pull 13 blobbed parts per page
// (plan condition: >=1 blob read/page avg). Content-addressed keys (sha256
// of the full object) are IMMUTABLE => exact by construction: no epoch, no
// TTL, a sha can never change meaning. Byte-capped FIFO at 32 MiB default
// (REFINE_BLOB_CACHE_MB; 0 disables; entries larger than the cap are not
// cached so one big object can never evict the working set).
use std::collections::{HashMap, VecDeque};

static BLOB_CACHE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct BlobCache {
    map: HashMap<String, std::sync::Arc<Vec<u8>>>,
    order: VecDeque<(String, usize)>,
    bytes: usize,
}

impl BlobCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }
    fn get(&mut self, sha: &str) -> Option<std::sync::Arc<Vec<u8>>> {
        // FIFO (no touch): insertion order approximates recency here — the
        // workload replays a small stable set of blobs per page.
        self.map.get(sha).map(std::sync::Arc::clone)
    }
    fn put(&mut self, sha: String, val: std::sync::Arc<Vec<u8>>, cap: usize) {
        let len = val.len();
        if len > cap {
            return;
        }
        if self.map.contains_key(&sha) {
            return; // immutable — same sha is the same bytes
        }
        while self.bytes + len > cap {
            match self.order.pop_front() {
                Some((old, n)) => {
                    self.bytes = self.bytes.saturating_sub(n);
                    self.map.remove(&old);
                }
                None => break,
            }
        }
        self.bytes += len;
        self.order.push_back((sha.clone(), len));
        self.map.insert(sha, val);
    }
    fn len(&self) -> usize {
        self.map.len()
    }
}

fn blob_cache() -> &'static parking_lot::Mutex<BlobCache> {
    static C: std::sync::OnceLock<parking_lot::Mutex<BlobCache>> = std::sync::OnceLock::new();
    C.get_or_init(|| parking_lot::Mutex::new(BlobCache::new()))
}

/// Per-call env read (~ns-scale; enables the kill switch without process
/// restart in tests). Unset => 32 MiB; parse failure => 32 MiB.
fn blob_cache_cap_bytes() -> usize {
    std::env::var("REFINE_BLOB_CACHE_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(32)
        * 1024
        * 1024
}

/// Test/observability: current cache occupancy (entries) and hits.
pub fn blob_cache_stats() -> (usize, u64) {
    (
        blob_cache().lock().len(),
        BLOB_CACHE_HITS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

pub struct BlobStore {
    root: PathBuf,
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Atomic publish: write tmp in same dir → fsync → rename → fsync dir (crash protocol).
fn atomic_write(final_path: &Path, data: &[u8]) -> Result<()> {
    let dir = final_path.parent().context("blob path has parent")?;
    fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let tmp = dir.join(format!(".tmp-{}", hex_digest(data)));
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, final_path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), final_path.display()))?;
    // persist the directory entry
    let df = fs::File::open(dir).with_context(|| format!("open dir {}", dir.display()))?;
    df.sync_all().ok(); // not supported on all FS; best-effort
    Ok(())
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| format!("mkdir {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn chunk_path(&self, obj_sha: &str, ord: usize) -> PathBuf {
        self.root
            .join("chunks")
            .join(&obj_sha[0..2])
            .join(obj_sha)
            .join(format!("{ord:08}"))
    }

    /// Store a logical object from a reader. Returns (sha256, byte_len, chunk_cnt).
    /// Reads in CHUNK_BYTES windows: a 122 MB row is pumped through here without ever
    /// being resident (STORAGE.md §5).
    pub fn put_from_reader<R: Read>(&self, mut src: R) -> Result<(String, u64, u32)> {
        let mut buf = vec![0u8; CHUNK_BYTES];
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let mut ord: usize = 0;
        // first pass impossible without knowing sha ahead; chunk paths keyed by
        // sha of content — so we stage chunks in a temp object dir keyed by random,
        // then rename to final dir after full hash known.
        let staging = self.root.join("chunks").join(".staging").join(hex_digest(
            &std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_le_bytes(),
        ));
        fs::create_dir_all(&staging)?;
        let mut written: Vec<(usize, PathBuf, usize)> = Vec::new();
        loop {
            let mut filled = 0;
            while filled < buf.len() {
                let n = src
                    .read(&mut buf[filled..])
                    .with_context(|| "blob read window")?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 && ord > 0 {
                break;
            }
            if filled == 0 && ord == 0 {
                // empty object: one empty chunk keeps indexing uniform
                let p = staging.join("00000000");
                atomic_write(&p, &[])?;
                written.push((0, p, 0));
                total = 0;
                break;
            }
            hasher.update(&buf[..filled]);
            total += filled as u64;
            let comp = zstd::bulk::compress(&buf[..filled], ZSTD_LEVEL)
                .with_context(|| "zstd compress chunk")?;
            let p = staging.join(format!("{ord:08}"));
            atomic_write(&p, &comp)?;
            written.push((ord, p, filled));
            ord += 1;
            if filled < buf.len() {
                break;
            }
        }
        let sha = hex::encode(hasher.finalize());
        // publish: staging dir -> final object dir (rename per-file; then drop staging)
        let obj_dir = self.root.join("chunks").join(&sha[0..2]).join(&sha);
        fs::create_dir_all(&obj_dir)?;
        for (ord, p, _) in &written {
            let dst = obj_dir.join(format!("{ord:08}"));
            if !dst.exists() {
                fs::rename(p, &dst).with_context(|| format!("publish chunk {ord} for {sha}"))?;
            } else {
                let _ = fs::remove_file(p);
            }
        }
        let _ = fs::remove_dir_all(&staging);
        Ok((sha, total, written.len() as u32))
    }

    /// Store bytes (small payloads). Thin wrapper over put_from_reader.
    pub fn put(&self, data: &[u8]) -> Result<(String, u64, u32)> {
        self.put_from_reader(std::io::Cursor::new(data))
    }

    /// Read an object fully into `sink`. Verifies chunk byte lengths.
    pub fn get_into<W: Write>(&self, sha: &str, expect_len: u64, sink: &mut W) -> Result<()> {
        let obj_dir = self.root.join("chunks").join(&sha[0..2]).join(sha);
        if !obj_dir.exists() {
            bail!("blob missing: {sha}");
        }
        let mut ord = 0usize;
        let mut total = 0u64;
        loop {
            let p = self.chunk_path(sha, ord);
            if !p.exists() {
                break;
            }
            let comp = fs::read(&p).with_context(|| format!("read chunk {ord} of {sha}"))?;
            let raw =
                zstd::bulk::decompress(&comp, CHUNK_BYTES).context("zstd decompress chunk")?;
            sink.write_all(&raw)?;
            total += raw.len() as u64;
            ord += 1;
            if ord > 1_000_000 {
                bail!("chunk runaway for {sha}");
            }
        }
        if total != expect_len {
            bail!("blob {sha} length mismatch: got {total}, want {expect_len}");
        }
        sink.flush()?;
        Ok(())
    }

    pub fn get(&self, sha: &str, expect_len: u64) -> Result<Vec<u8>> {
        let cap = blob_cache_cap_bytes();
        if cap > 0
            && let Some(v) = blob_cache().lock().get(sha)
        {
            BLOB_CACHE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok((*v).clone());
        }
        let mut out = Vec::with_capacity(expect_len.min(MAX_ALLOC as u64) as usize);
        self.get_into(sha, expect_len, &mut out)?;
        if cap > 0 && out.len() <= cap {
            blob_cache()
                .lock()
                .put(sha.to_string(), std::sync::Arc::new(out.clone()), cap);
        }
        Ok(out)
    }

    /// Boot-time orphan sweep: chunk dirs with no live blob_object row are deleted.
    /// `live` closure answers from the DB (writer transaction already committed —
    /// orphans are by definition unreferenced). Grace-period deferred to caller.
    pub fn gc_orphans<F: Fn(&str) -> bool>(&self, live: F) -> Result<usize> {
        let chunks = self.root.join("chunks");
        let mut removed = 0;
        let entries = match fs::read_dir(&chunks) {
            Ok(e) => e,
            Err(_) => return Ok(0),
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue; // staging
            }
            let pre = match fs::read_dir(e.path()) {
                Ok(p) => p,
                Err(_) => continue,
            };
            for obj in pre.flatten() {
                let sha = obj.file_name().to_string_lossy().to_string();
                if sha.starts_with('.') || sha.len() != 64 {
                    continue;
                }
                if !live(&sha) {
                    fs::remove_dir_all(obj.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small() {
        let d = tempfile::tempdir().unwrap();
        let bs = BlobStore::new(d.path()).unwrap();
        let (sha, len, cnt) = bs.put(b"hello world").unwrap();
        assert_eq!(len, 11);
        assert_eq!(cnt, 1);
        assert_eq!(bs.get(&sha, len).unwrap(), b"hello world");
    }

    #[test]
    fn roundtrip_empty_and_boundary() {
        let d = tempfile::tempdir().unwrap();
        let bs = BlobStore::new(d.path()).unwrap();
        let (sha, len, _) = bs.put(b"").unwrap();
        assert_eq!(len, 0);
        assert_eq!(bs.get(&sha, 0).unwrap(), b"");

        for &n in &[1usize, CHUNK_BYTES - 1, CHUNK_BYTES, CHUNK_BYTES + 1] {
            let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
            let (sha, len, _) = bs.put(&data).unwrap();
            assert_eq!(len, n as u64);
            assert_eq!(bs.get(&sha, len).unwrap(), data, "roundtrip {n}");
        }
    }

    #[test]
    fn roundtrip_multi_chunk_above_alloc_cap() {
        // property: arbitrary sizes (proptest-style sizes incl. just over 8 MiB cap
        // never allocate the whole object in get() — we stream into a sink here)
        let d = tempfile::tempdir().unwrap();
        let bs = BlobStore::new(d.path()).unwrap();
        let n = CHUNK_BYTES * 3 + 17;
        let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        let (sha, len, cnt) = bs.put(&data).unwrap();
        assert_eq!(cnt, 4);
        let mut out = Vec::new();
        bs.get_into(&sha, len, &mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn missing_blob_and_length_mismatch_fail_loudly() {
        let d = tempfile::tempdir().unwrap();
        let bs = BlobStore::new(d.path()).unwrap();
        assert!(bs.get("ab".repeat(32).as_str(), 5).is_err());
        let (sha, len, _) = bs.put(b"abc").unwrap();
        assert!(bs.get(&sha, len + 1).is_err(), "length mismatch must error");
    }

    #[test]
    fn content_addressed_dedupe() {
        let d = tempfile::tempdir().unwrap();
        let bs = BlobStore::new(d.path()).unwrap();
        let (s1, _, _) = bs.put(b"same").unwrap();
        let (s2, _, _) = bs.put(b"same").unwrap();
        assert_eq!(s1, s2);
    }

    #[test]
    fn gc_removes_unreferenced_keeps_live() {
        let d = tempfile::tempdir().unwrap();
        let bs = BlobStore::new(d.path()).unwrap();
        let (live, _, _) = bs.put(b"keep me").unwrap();
        let (dead, _, _) = bs.put(b"delete me").unwrap();
        let removed = bs.gc_orphans(|sha| sha == live).unwrap();
        assert_eq!(removed, 1);
        assert!(bs.get(&live, 7).is_ok());
        assert!(bs.get(&dead, 9).is_err(), "orphan must be gone");
    }

    /// PERF-10X B1 end-to-end: a second get() after the chunks are GONE
    /// must still return byte-identical content — only possible if the
    /// cache served it (disk path would error "blob missing").
    #[test]
    fn cached_get_survives_chunk_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let bs = BlobStore::new(dir.path()).unwrap();
        let payload = b"B1 cache proof payload".to_vec();
        let (sha, len, _) = bs.put(&payload).unwrap();
        let first = bs.get(&sha, len).unwrap();
        assert_eq!(first, payload);
        // remove the object from disk entirely
        let obj_dir = dir.path().join("chunks").join(&sha[0..2]).join(&sha);
        std::fs::remove_dir_all(&obj_dir).unwrap();
        let second = bs.get(&sha, len).unwrap();
        assert_eq!(second, payload, "second get must be served from cache");
    }

    /// Eviction contract: byte cap respected, FIFO order, oversized entries
    /// never admitted (so one big object cannot evict the working set).
    #[test]
    fn blob_cache_evicts_fifo_and_rejects_oversized() {
        let mut c = BlobCache::new();
        c.put("a".into(), std::sync::Arc::new(vec![0u8; 60]), 100);
        c.put("b".into(), std::sync::Arc::new(vec![0u8; 60]), 100);
        assert!(c.get("a").is_none(), "oldest evicted at cap");
        assert!(c.get("b").is_some());
        assert!(c.bytes <= 100, "byte accounting respected: {}", c.bytes);
        c.put("big".into(), std::sync::Arc::new(vec![0u8; 101]), 100);
        assert!(c.get("big").is_none(), "oversized entry not admitted");
        assert_eq!(c.len(), 1);
        // same sha twice = immutable, single entry
        c.put("b".into(), std::sync::Arc::new(vec![1u8; 60]), 100);
        assert_eq!(c.len(), 1, "same-sha put is a no-op");
    }
}
