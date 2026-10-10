//! Blob-store concurrency + boundary regression (2026-10-11 deeper bug hunt).
//!
//! The staging directory under `chunks/.staging/` was keyed on the wall-clock
//! nanoseconds alone; two concurrent `put_from_reader` calls landing on the
//! same clock tick (or a coarse-clock kernel) would stage into the SAME dir
//! and could rename each other's chunks into the wrong content-addressed
//! object (one object would get the other's chunk bytes → a length/content
//! mismatch on read, or a silently wrong blob). The key is now
//! pid+nanos+counter; this test hammers concurrent puts and verifies every
//! object reads back byte-exact.
//!
//! Also exercises the inline/blob boundary (INLINE_PART_MAX = 8 KiB): a part
//! one byte over the cap must round-trip through the blob store exactly.
use ocserve_store::BlobStore;
use std::sync::Arc;

#[test]
fn concurrent_puts_are_content_addressed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(BlobStore::new(dir.path().join("blobs")).unwrap());
    let mut handles = Vec::new();
    // distinct payloads of a few chunks each, put concurrently
    for t in 0..16u8 {
        let store = Arc::clone(&store);
        handles.push(std::thread::spawn(move || {
            let mut shas = Vec::new();
            for n in 0..8u32 {
                // ~1.5 MiB so each object spans multiple chunks
                let mut payload = vec![t; 1_500_000];
                payload[0] = t;
                payload[1] = n as u8;
                let (sha, len, chunks) = store.put(&payload).unwrap();
                assert_eq!(len as usize, payload.len());
                assert!(chunks >= 2, "multi-chunk: {chunks}");
                shas.push((sha, payload));
            }
            shas
        }));
    }
    let mut expected: Vec<(String, Vec<u8>)> = Vec::new();
    for h in handles {
        expected.extend(h.join().unwrap());
    }
    // every object must read back byte-exact (no cross-publish)
    for (sha, want) in &expected {
        let got = store.get(sha, want.len() as u64).unwrap();
        assert_eq!(&got, want, "sha {sha} content mismatch");
    }
}

#[test]
fn inline_boundary_roundtrips_through_blob() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs")).unwrap();
    // exactly at and one past the inline cap (8 KiB)
    for n in [
        ocserve_store::INLINE_PART_MAX,
        ocserve_store::INLINE_PART_MAX + 1,
    ] {
        let payload = vec![b'x'; n];
        let (sha, len, _) = store.put(&payload).unwrap();
        assert_eq!(len as usize, n);
        assert_eq!(store.get(&sha, n as u64).unwrap(), payload, "len {n}");
    }
    // empty object still round-trips (one empty chunk)
    let (sha, len, _) = store.put(b"").unwrap();
    assert_eq!(len, 0);
    assert_eq!(store.get(&sha, 0).unwrap(), Vec::<u8>::new());
}

#[test]
fn content_hash_is_stable_across_calls() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path().join("blobs")).unwrap();
    let payload = vec![7u8; 100_000];
    let (a, _, _) = store.put(&payload).unwrap();
    let (b, _, _) = store.put(&payload).unwrap();
    assert_eq!(a, b, "identical content → identical sha (dedup)");
}
