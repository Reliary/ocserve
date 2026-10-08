//! Pty connect tickets — core/src/pty/ticket.ts parity: UUID v4 ticket,
//! 60 s TTL, single use, scoped (ptyID + directory), capacity-bounded.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_TTL: Duration = Duration::from_secs(60);
pub const CAPACITY: usize = 10_000;

struct Record {
    pty_id: String,
    directory: String,
    issued: Instant,
}

pub struct TicketStore {
    inner: Mutex<HashMap<String, Record>>,
}

impl TicketStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn issue(&self, pty_id: &str, directory: &str) -> (String, u64) {
        let ticket = uuid_v4();
        let mut inner = self.inner.lock().unwrap();
        inner.retain(|_, r| r.issued.elapsed() < DEFAULT_TTL);
        if inner.len() >= CAPACITY {
            // bounded: drop the oldest entry
            if let Some(oldest) = inner
                .iter()
                .min_by_key(|(_, r)| r.issued)
                .map(|(k, _)| k.clone())
            {
                inner.remove(&oldest);
            }
        }
        inner.insert(
            ticket.clone(),
            Record {
                pty_id: pty_id.to_string(),
                directory: directory.to_string(),
                issued: Instant::now(),
            },
        );
        (ticket, DEFAULT_TTL.as_secs())
    }

    /// Single-use, scope-checked consume. Upstream semantics
    /// (`Cache.invalidateWhen`): the entry is only invalidated when the
    /// predicate matches — a scope mismatch leaves the ticket intact.
    pub fn consume(&self, ticket: &str, pty_id: &str, directory: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.get(ticket) {
            None => false,
            Some(r) if r.issued.elapsed() >= DEFAULT_TTL => {
                inner.remove(ticket);
                false
            }
            Some(r) if r.pty_id == pty_id && r.directory == directory => {
                inner.remove(ticket);
                true
            }
            Some(_) => false,
        }
    }
}

impl Default for TicketStore {
    fn default() -> Self {
        Self::new()
    }
}

/// RFC 4122 v4 from the OS CSPRNG (/dev/urandom), falling back to
/// RandomState on read failure.
fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| {
            use std::io::Read as _;
            f.read_exact(&mut bytes)
        })
        .is_ok();
    if !ok {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        let mut h = RandomState::new().build_hasher();
        h.write_u64(Instant::now().elapsed().as_nanos() as u64);
        let a = h.finish();
        h.write_u64(a);
        let b = h.finish();
        bytes[..8].copy_from_slice(&a.to_le_bytes());
        bytes[8..].copy_from_slice(&b.to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10xx
    let h = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        h(&bytes[0..4]),
        h(&bytes[4..6]),
        h(&bytes[6..8]),
        h(&bytes[8..10]),
        h(&bytes[10..16])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_consume_scope_and_single_use() {
        let s = TicketStore::new();
        let (t, exp) = s.issue("pty_a", "/dir");
        assert_eq!(exp, 60);
        assert!(t.contains('-'), "uuid shape: {t}");
        // wrong scope: not consumed (upstream invalidateWhen keeps it)
        assert!(!s.consume(&t, "pty_b", "/dir"));
        assert!(!s.consume(&t, "pty_a", "/other"));
        // still valid for the right scope, and single-use after that
        assert!(s.consume(&t, "pty_a", "/dir"));
        assert!(!s.consume(&t, "pty_a", "/dir"));
    }

    #[test]
    fn unknown_ticket_false() {
        let s = TicketStore::new();
        assert!(!s.consume("nope", "pty_a", "/dir"));
    }
}
