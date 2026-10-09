//! Upstream-shaped resource IDs: `<prefix>_<26 chars>` where the first 10 hex
//! chars encode ms-timestamp LE (matches observed `msg_0fa1e3f770014jHIfQNh0Lhiu3`).

use sha2::{Digest, Sha256};

fn encode(ms: u64, rand_tail: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(ms.to_le_bytes());
    h.update(rand_tail);
    let hex = hex::encode(h.finalize());
    // upstream body = 26 chars: 13-char time hex + 13-char hash slice
    // (observed `msg_0fa1e3f770014jHIfQNh0Lhiu3`)
    let time_hex = format!("{ms:013x}"); // pads; current ms fits 13
    format!("{time_hex}{}", &hex[..13])
}

pub fn msg_id() -> String {
    let ms = now_ms();
    let nonce: Vec<u8> = std::iter::repeat_with(rand_byte).take(8).collect();
    format!("msg_{}", encode(ms, &nonce[..8]))
}

pub fn prt_id() -> String {
    let ms = now_ms();
    let nonce: Vec<u8> = std::iter::repeat_with(rand_byte).take(8).collect();
    format!("prt_{}", encode(ms, &nonce[..8]))
}

pub fn ses_id() -> String {
    let ms = now_ms();
    let nonce: Vec<u8> = std::iter::repeat_with(rand_byte).take(8).collect();
    format!("ses_{}", encode(ms, &nonce[..8]))
}

/// Question request id (v1 QuestionID: `que_` +26 chars, ascending).
pub fn que_id() -> String {
    let ms = now_ms();
    let nonce: Vec<u8> = std::iter::repeat_with(rand_byte).take(8).collect();
    format!("que_{}", encode(ms, &nonce[..8]))
}

/// Permission request id (v1 PermissionID: `per_` +26 chars). Events must carry
/// the `per_` id — the spec pattern `^per` and clients key on it (2026-10-09:
/// ocserve emitted `evt_` ids, failing the event-shape guard).
pub fn per_id() -> String {
    let ms = now_ms();
    let nonce: Vec<u8> = std::iter::repeat_with(rand_byte).take(8).collect();
    format!("per_{}", encode(ms, &nonce[..8]))
}

/// Upstream `Identifier.ascending()` tail: 13 hex chars = ms*4096+counter
/// encoded big-endian in 6 bytes; +12 random base62. Used for pty_ ids
/// (schema/src/identifier.ts, procced live 2026-10-08).
pub fn ascending_tail() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ms = now_ms();
    let prev = LAST_MS.swap(ms, Ordering::Relaxed);
    if prev != ms {
        COUNTER.store(0, Ordering::Relaxed);
    }
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    let current = ms.wrapping_mul(0x1000).wrapping_add(counter);
    let mut time = String::with_capacity(12);
    for i in 0..6 {
        let byte = (current >> (40 - 8 * i)) & 0xff;
        time.push_str(&format!("{byte:02x}"));
    }
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut tail = String::with_capacity(14);
    for _ in 0..14 {
        tail.push(CHARS[rand_byte() as usize % 62] as char);
    }
    format!("{time}{tail}")
}

pub fn evt_id() -> String {
    let ms = now_ms();
    let nonce: Vec<u8> = std::iter::repeat_with(rand_byte).take(8).collect();
    format!("evt_{}", encode(ms, &nonce[..8]))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn rand_byte() -> u8 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u64(now_ms());
    h.finish() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_upstream_shape() {
        for id in [msg_id(), prt_id(), ses_id(), evt_id()] {
            let (prefix, body) = id.split_once('_').unwrap();
            assert!(matches!(prefix, "msg" | "prt" | "ses" | "evt"));
            assert_eq!(body.len(), 26, "upstream ids are prefix+26: {id}");
            assert!(body.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
        }
    }

    #[test]
    fn ids_unique_enough() {
        let a = msg_id();
        let b = msg_id();
        assert_ne!(a, b, "collision within a millisecond: {a}");
    }
}
