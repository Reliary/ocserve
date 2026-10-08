//! Zero-allocation JSON compaction + column splice for the read path
//! (PERF-10X Phase II L1).
//!
//! # Why this exists
//!
//! Phase I (`bench/perf/PHASE1-ATTRIBUTION.md`) attributed **86% of read-path
//! allocations to serde_json DOM work**: every `msg.info` and every
//! `msg_part.inline` blob is parsed into a `Value`, three column keys are
//! injected, and the tree is re-serialized. Measured cost of a 50-message
//! page: **1,249 µs of JSON against 26 µs of SQL**. Per iteration of the
//! production mix that was 11,900 allocations and 4.67 MB, of which
//! `serde_json` string allocation was 46.5% and `ValueVisitor::visit_map`
//! 21.7%.
//!
//! # Why it is not a raw passthrough
//!
//! The DOM path *normalizes* on the way out:
//! - serde's compact formatter writes `,` and `":"` with no whitespace
//!   (`serde_json-1.0.150/src/ser.rs:1884-1893`) — upstream's Bun writer
//!   emits `": "`/`", "` for 39% of stored payloads;
//! - escapes are normalized (`\/`→`/`, `\u0041`→`A`);
//! - numbers are re-formatted through `ryu` (`1.10`→`1.1`, `1e3`→`1000.0`).
//!
//! Upstream normalizes the same way (JS `JSON.parse` + `JSON.stringify`), so
//! the recorded wire corpus is *compact*. Passing stored bytes through
//! verbatim would therefore change bytes on the wire for 39% of rows — a
//! wire-compat violation, not an optimization. Hence: a **streaming
//! compactor**, not a byte copy.
//!
//! # What it actually does
//!
//! One pass over the input, writing into a caller-owned `Vec<u8>`:
//! - structure: insignificant whitespace outside strings is dropped, commas
//!   and colons are re-emitted compactly;
//! - strings: verbatim bytes unless they contain a non-canonical escape, in
//!   which case serde normalizes that one string (rare; delegated, never
//!   hand-rolled);
//! - numbers: verbatim when already canonical (integers — the overwhelming
//!   majority), otherwise delegated to serde for exact `ryu` parity;
//! - keys: verbatim for the three column keys' *names*, values replaced with
//!   the authoritative column values (serde-identical escaping);
//! - the three column keys are replaced in place if the source carried them,
//!   or appended before the closing brace if it did not.
//!
//! # Contract
//!
//! `compact_splice(src, …) == dom_merge(src, …)` **byte-for-byte**, proven
//! over every stored `info`/`inline` row by `tests/splice_parity.rs` (corpus
//! differential, not a sample). Returns `None` when the input is not a
//! top-level object that can be proven — the caller then runs the DOM path,
//! so this module is an optimization that can never change semantics.

/// Column keys `merge_columns` injects. Columns are authoritative over the
/// stored blob (upstream serving semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Id = 0,
    Session = 1,
    Message = 2,
}

impl Col {
    const ALL: [Col; 3] = [Col::Id, Col::Session, Col::Message];

    fn name(self) -> &'static str {
        match self {
            Col::Id => "id",
            Col::Session => "sessionID",
            Col::Message => "messageID",
        }
    }

    fn of(key: &str) -> Option<Col> {
        match key {
            "id" => Some(Col::Id),
            "sessionID" => Some(Col::Session),
            "messageID" => Some(Col::Message),
            _ => None,
        }
    }
}

/// serde's `write_string_fragment` escaping, byte-identical to
/// `serde_json::to_writer` for a `Value::String`: quote, backslash, and the
/// short escapes serde uses, `\u00xx` (lowercase hex) for other C0 controls,
/// everything else — including `/` and all non-ASCII — as UTF-8.
pub fn push_json_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    let bytes = s.as_bytes();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            0x08 => b"\\b",
            0x0c => b"\\f",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x00..=0x1f => b"",
            _ => continue,
        };
        out.extend_from_slice(&bytes[start..i]);
        if esc.is_empty() {
            out.extend_from_slice(format!("\\u{:04x}", b as u32).as_bytes());
        } else {
            out.extend_from_slice(esc);
        }
        start = i + 1;
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

/// Scan a JSON string starting at `b[start] == b'"'`; returns the index one
/// past the closing quote.
fn scan_string(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Caller-owned scratch for one `compact_splice` call: a reusable hash set
/// of the top-level keys already seen, plus one slot for a decoded escaped
/// key. Reusing it across rows is what keeps the splice allocation-free —
/// a per-row `HashSet` plus a per-key `String` were 14.2% + 36.3% of
/// allocations in the first post-L1 profile (bytes allocated 2.1%, so it is
/// a *count* problem, not a volume one).
#[derive(Default)]
pub struct SpliceScratch {
    seen: std::collections::HashSet<u64>,
    decoded_key: String,
}

impl SpliceScratch {
    /// Drop every recorded key. Callers hold one scratch per fetch.
    pub fn clear(&mut self) {
        self.seen.clear();
    }

    /// True if this key hash was NOT already recorded (i.e. insert is new).
    fn seen(&mut self, hash: u64) -> bool {
        self.seen.insert(hash)
    }

    /// Stash a decoded escaped key and borrow it (valid until the next
    /// `put_key`/`clear`).
    fn put_key(&mut self, k: String) -> Option<&str> {
        self.decoded_key.clear();
        self.decoded_key.push_str(&k);
        Some(self.decoded_key.as_str())
    }
}

/// FNV-1a over a key: only used for duplicate detection, so a collision is
/// harmless (it can only cause a conservative refusal).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Non-canonical string escapes that serde rewrites (`\/` → `/`,
/// `\uXXXX` → literal when it is a plain BMP char, `\uD8xx` surrogates
/// rejected). If a string contains any of these we delegate that single
/// string to serde rather than reimplementing its rules.
fn needs_string_normalize(raw: &[u8]) -> bool {
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] == b'\\' {
            match raw.get(i + 1) {
                Some(b'/') | Some(b'u') => return true,
                _ => i += 2,
            }
        } else {
            i += 1;
        }
    }
    false
}

/// Number tokens that are already byte-identical to serde's output: plain
/// integers with no `+`, leading zeros, `.` or exponent. Anything else is
/// delegated to serde so `ryu` formatting matches exactly.
fn is_canonical_number(tok: &[u8]) -> bool {
    if tok.is_empty() {
        return false;
    }
    let digits = if tok[0] == b'-' { &tok[1..] } else { tok };
    let plain = match digits {
        // "-0" canonicalizes to 0 in serde; not canonical
        [b'0'] => tok[0] != b'-',
        [first, rest @ ..] if first.is_ascii_digit() && *first != b'0' => {
            rest.iter().all(u8::is_ascii_digit)
        }
        _ => false,
    };
    // A plain integer is byte-identical to serde's output ONLY while serde
    // keeps it an integer: `123456789012345678901234567890` overflows i64 and
    // serde re-emits `1.2345678901234568e+29`. Delegating every >i64::MAX
    // token keeps parity at the cost of one serde call on absurd values that
    // never occur in stored payloads.
    plain && digits.len() <= 19
}

/// Compact-copy one JSON value starting at `b[pos]` into `out`, dropping
/// insignificant whitespace and normalizing strings/numbers exactly as serde
/// would. Returns the index one past the value's last byte — for containers
/// that is its closing bracket; for scalars it is the byte that DELIMITS it
/// (`,`, `}`, `]`, or end of input), which belongs to the caller.
///
/// Two design rules that four bug iterations taught (all silent corruption,
/// not refusals — see `tests/splice_parity.rs`):
///
/// 1. **The caller owns delimiters.** A nested call never consumes the `,`
///    or `}` that terminates it, or sibling values merge: `[1,2]` → `[12]`,
///    `{"start":1,"end":2}` → `{"start":1"end":2]`.
/// 2. **Bracket matching uses a stack of openers**, not a depth counter. A
///    counter decrements on any closer, so `[1,2]` inside an object closed the
///    object early (`{"a":[1,2}` truncated).
///
/// Object members run an explicit four-state machine (Key → Colon → Value →
/// Comma) because the earlier `expect_key`/`after_value` pair lost the comma
/// in `{"a":1,"b":2}`.
fn emit_value(b: &[u8], pos: usize, out: &mut Vec<u8>) -> Option<usize> {
    let mut i = pos;
    while i < b.len() && (b[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    match *b.get(i)? {
        open @ (b'{' | b'[') => {
            let is_obj = open == b'{';
            let close = if is_obj { b'}' } else { b']' };
            out.push(open);
            i += 1;
            #[derive(PartialEq, Clone, Copy)]
            enum Phase {
                Key,
                Colon,
                Value,
                Comma,
            }
            let mut phase = if is_obj { Phase::Key } else { Phase::Value };
            // `{}` is legal; `{"a":1,}` is not. Both arrive with phase == Key,
            // so `saw_member` is what separates them — without it every EMPTY
            // nested object was refused, which is 4,678 stored tool-state rows
            // in the fixture corpus (the `input: {}` shape).
            let mut saw_member = false;
            loop {
                while i < b.len() && (b[i] as char).is_ascii_whitespace() {
                    i += 1;
                }
                match *b.get(i)? {
                    c if c == close => {
                        if is_obj && phase == Phase::Key && saw_member {
                            return None; // `{"a":1,}` — trailing comma
                        }
                        out.push(close);
                        return Some(i + 1);
                    }
                    c if phase == Phase::Comma => {
                        if c != b',' {
                            return None; // missing comma between elements
                        }
                        out.push(b',');
                        i += 1;
                        phase = if is_obj { Phase::Key } else { Phase::Value };
                    }
                    c if phase == Phase::Key => {
                        if c != b'"' {
                            return None; // object key must be a string
                        }
                        i = emit_value(b, i, out)?;
                        phase = Phase::Colon;
                    }
                    c if phase == Phase::Colon => {
                        if c != b':' {
                            return None;
                        }
                        out.push(b':');
                        i += 1;
                        saw_member = true;
                        phase = Phase::Value;
                    }
                    c => {
                        // Value: reject a stray comma or an early close
                        if c == b',' || c == b'}' || c == b']' {
                            return None;
                        }
                        i = emit_value(b, i, out)?;
                        phase = Phase::Comma;
                    }
                }
            }
        }
        b'"' => {
            let end = scan_string(b, i)?;
            let raw = &b[i..end];
            if needs_string_normalize(&raw[1..raw.len() - 1]) {
                let v: serde_json::Value =
                    serde_json::from_str(std::str::from_utf8(raw).ok()?).ok()?;
                serde_json::to_writer(out, &v).ok()?;
            } else {
                out.extend_from_slice(raw);
            }
            Some(end)
        }
        b't' | b'n' | b'f' => {
            for lit in ["true", "null", "false"] {
                if b[i..].starts_with(lit.as_bytes()) {
                    out.extend_from_slice(lit.as_bytes());
                    return Some(i + lit.len());
                }
            }
            None
        }
        _ => {
            // Scalar: take exactly one token, never the delimiter after it,
            // and validate it — copying unvalidated bytes let bare words
            // through, so the output was no longer the same JSON as the input.
            let tok_end = b[i..]
                .iter()
                .position(|&c| matches!(c, b',' | b'}' | b']' | b' ' | b'\n' | b'\t' | b'\r'))
                .map(|p| i + p)
                .unwrap_or(b.len());
            if tok_end == i {
                return None;
            }
            let tok = std::str::from_utf8(&b[i..tok_end]).ok()?;
            if is_canonical_number(tok.as_bytes()) {
                out.extend_from_slice(tok.as_bytes());
            } else {
                // non-canonical number (1.10 → 1.1, 1e3 → 1000.0, -0 → -0.0)
                // or a bare word: delegate for exact serde/ryu parity
                let v: serde_json::Value = serde_json::from_str(tok).ok()?;
                serde_json::to_writer(out, &v).ok()?;
            }
            Some(tok_end)
        }
    }
}

/// Advance past one JSON value without emitting it (used when replacing a
/// column key's value, which we re-emit ourselves).
fn skip_value(b: &[u8], pos: usize) -> Option<usize> {
    let mut i = pos;
    while i < b.len() && (b[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    match b.get(i)? {
        b'{' | b'[' => {
            let mut depth = 0i32;
            while i < b.len() {
                match b[i] {
                    b'"' => i = scan_string(b, i)?,
                    b'{' | b'[' => {
                        depth += 1;
                        i += 1;
                    }
                    b'}' | b']' => {
                        depth -= 1;
                        i += 1;
                        if depth == 0 {
                            return Some(i);
                        }
                    }
                    _ => i += 1,
                }
            }
            None
        }
        b'"' => scan_string(b, i),
        b't' | b'n' | b'f' => {
            for lit in ["true", "null", "false"] {
                if b[i..].starts_with(lit.as_bytes()) {
                    return Some(i + lit.len());
                }
            }
            None
        }
        _ => {
            let start = i;
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']' | b' ' | b'\n' | b'\t' | b'\r')
            {
                i += 1;
            }
            if i == start { None } else { Some(i) }
        }
    }
}

/// Compact a stored JSON object and splice in the authoritative column keys.
///
/// `message` is `None` for `msg.info` and `Some(part_message_id)` for parts,
/// matching `merge_columns`. `out` is cleared and reused — the caller owns
/// the buffer, so no allocation happens per message on the hot path.
///
/// Returns `false` when the input is not a provable top-level object; the
/// caller must then use the DOM path. Never partially writes: on failure the
/// buffer is left as it was.
pub fn compact_splice(
    src: &str,
    id: &str,
    session_id: &str,
    message: Option<&str>,
    out: &mut Vec<u8>,
    scratch: &mut SpliceScratch,
) -> bool {
    let b = src.as_bytes();
    let mut i = 0usize;
    while i < b.len() && (b[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    if b.get(i) != Some(&b'{') {
        return false;
    }
    let mut work: Vec<u8> = std::mem::take(out);
    work.clear();
    work.push(b'{');
    i += 1;
    let mut seen = [false; 3];
    // Duplicate top-level keys deserialize differently under serde (last
    // wins) than they appear in the bytes; refuse rather than emit bytes
    // whose meaning changed. Caller-provided scratch: a fresh HashSet per
    // row was 14.2% of all allocations in the post-L1 profile
    // (hashbrown reserve_rehash) on rows that never have a duplicate.
    scratch.clear();
    let mut first = true;
    let mut ok = true;

    loop {
        while i < b.len() && (b[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        match b.get(i) {
            None => {
                ok = false;
                break;
            }
            Some(b'}') => {
                i += 1;
                break;
            }
            Some(b',') if !first => {
                i += 1;
                continue;
            }
            Some(b'"') => {}
            Some(_) => {
                ok = false;
                break;
            }
        }
        let kstart = i;
        let raw_key = match scan_string(b, i) {
            Some(e) => &b[i..e],
            None => {
                ok = false;
                break;
            }
        };
        i += raw_key.len();
        while i < b.len() && (b[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        if b.get(i) != Some(&b':') {
            ok = false;
            break;
        }
        // CRITICAL: remember where the ':' sits. `kstart..i` is copied
        // verbatim for untouched members, so if whitespace followed the
        // colon it would be copied into a compact output and break byte
        // parity (caught by the DOM oracle on the first corpus shape,
        // `{"type":"text", "text":…}`).
        let colon = i;
        i += 1;

        // Borrowed key bytes — no String per member. The escaped-key branch
        // is the only one that allocates, and only when a key literally
        // contains a backslash (vanishingly rare in stored payloads), in
        // which case the decoded key lives in caller scratch.
        let key_ref: Option<&str> = if raw_key.contains(&b'\\') {
            let raw_key_str = match std::str::from_utf8(raw_key) {
                Ok(k) => k,
                Err(_) => {
                    ok = false;
                    break;
                }
            };
            match serde_json::from_str::<String>(raw_key_str) {
                Ok(k) => {
                    if !scratch.seen(fnv1a(k.as_bytes())) {
                        ok = false; // duplicate key — DOM path decides
                        break;
                    }
                    scratch.put_key(k)
                }
                Err(_) => None,
            }
        } else {
            match std::str::from_utf8(&raw_key[1..raw_key.len() - 1]) {
                Ok(k) => {
                    if !scratch.seen(fnv1a(k.as_bytes())) {
                        ok = false; // duplicate key — DOM path decides
                        break;
                    }
                    Some(k)
                }
                // a non-UTF8 key cannot appear in valid JSON
                Err(_) => None,
            }
        };
        let key: Option<Col> = key_ref.and_then(Col::of);

        if !first {
            work.push(b',');
        }
        first = false;
        match key {
            Some(col) => {
                seen[col as usize] = true;
                work.push(b'"');
                work.extend_from_slice(col.name().as_bytes());
                work.extend_from_slice(b"\":");
                let v = match col {
                    Col::Id => id,
                    Col::Session => session_id,
                    Col::Message => match message {
                        Some(m) => m,
                        None => {
                            ok = false;
                            break;
                        }
                    },
                };
                push_json_string(&mut work, v);
                // skip the stored value (no output needed)
                match skip_value(b, i) {
                    Some(next) => i = next,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            None => {
                work.extend_from_slice(&b[kstart..colon + 1]); // key + ':'
                i = match emit_value(b, i, &mut work) {
                    Some(next) => next,
                    None => {
                        ok = false;
                        break;
                    }
                };
            }
        }
    }

    if ok {
        for col in Col::ALL {
            if seen[col as usize] {
                continue;
            }
            let v = match col {
                Col::Id => id,
                Col::Session => session_id,
                Col::Message => match message {
                    Some(m) => m,
                    None => continue,
                },
            };
            // no leading comma when the source object was empty
            if !first {
                work.push(b',');
            }
            first = false;
            work.push(b'"');
            work.extend_from_slice(col.name().as_bytes());
            work.extend_from_slice(b"\":");
            push_json_string(&mut work, v);
        }
        while i < b.len() && (b[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        if i != b.len() {
            ok = false;
        }
    }
    if ok {
        work.push(b'}');
        *out = work;
    } else {
        // hand the caller's buffer back untouched (capacity preserved for
        // the DOM fallback path)
        *out = work;
    }
    ok
}

/// Compile-time-only helper kept for documentation symmetry with the DOM path:
/// the value the splice is required to reproduce.
#[cfg(test)]
pub fn dom_reference(src: &str, id: &str, session_id: &str, message: Option<&str>) -> String {
    let mut v: serde_json::Value = serde_json::from_str(src).expect("stored blob is JSON");
    v["id"] = serde_json::Value::String(id.to_string());
    v["sessionID"] = serde_json::Value::String(session_id.to_string());
    if let Some(m) = message {
        v["messageID"] = serde_json::Value::String(m.to_string());
    }
    serde_json::to_string(&v).expect("serializable")
}
