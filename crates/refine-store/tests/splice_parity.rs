//! Byte-parity contract for the zero-allocation splice (PERF-10X L1).
//!
//! Phase I attributed 86% of read-path allocations to serde_json DOM work
//! (`bench/perf/PHASE1-ATTRIBUTION.md`). The splice removes the DOM, which is
//! only safe if it reproduces the DOM output **byte for byte** — the wire
//! corpus is a byte-golden freeze, so a formatting difference is a wire
//! violation, not a cosmetic one.
//!
//! These tests are the contract:
//!   * `splice_matches_dom` — property test over adversarial inputs (spacing,
//!     escapes, floats, nesting, unicode, key order) proving
//!     `compact_splice == dom_merge` byte-for-byte, with the DOM as oracle;
//!   * `splice_refuses_malformed` — negative control: anything unprovable
//!     returns false so the caller falls back to the DOM;
//!   * `splice_parity_over_corpus` — **corpus differential over every stored
//!     row** when `REFINE_SPLICE_DB` points at a real refine.db (not a
//!     sample): every `msg.info` and every `msg_part.inline`, byte-compared
//!     against the DOM reference. This is the gate L1 must pass to ship.

use refine_store::splice::SpliceScratch;
use refine_store::splice::compact_splice;

fn dom_reference(src: &str, id: &str, session_id: &str, message: Option<&str>) -> String {
    let mut v: serde_json::Value = serde_json::from_str(src).expect("stored blob is JSON");
    v["id"] = serde_json::Value::String(id.to_string());
    v["sessionID"] = serde_json::Value::String(session_id.to_string());
    if let Some(m) = message {
        v["messageID"] = serde_json::Value::String(m.to_string());
    }
    serde_json::to_string(&v).expect("serializable")
}

fn assert_parity(src: &str, id: &str, sid: &str, mid: Option<&str>) {
    let mut out = Vec::with_capacity(512);
    let mut scratch = SpliceScratch::default();
    let ok = compact_splice(src, id, sid, mid, &mut out, &mut scratch);
    assert!(ok, "splice refused a VALID object: {src}");
    let got = String::from_utf8(out).expect("utf8");
    let want = dom_reference(src, id, sid, mid);
    assert_eq!(
        got, want,
        "splice != DOM\n  src: {src}\n  got: {got}\n want: {want}"
    );
}

#[test]
fn splice_matches_dom_on_shapes_that_broke_naive_passthrough() {
    let cases = [
        // upstream Bun spacing — DOM normalizes it away (39% of stored rows)
        r#"{"type":"text", "text":"hi", "time":{"start":1,"end":2}}"#,
        r#"{"a":1,"b":[1,2, 3],"c":{"d":null}}"#,
        // column keys already present: replaced in place, order preserved
        r#"{"id":"old","sessionID":"old","type":"text"}"#,
        r#"{"type":"text","id":"old"}"#,
        // partial presence → appended
        r#"{"sessionID":"old","x":true}"#,
        // empty object
        r#"{}"#,
        r#"{  }"#,
        // escapes: canonical must stay, non-canonical must normalize
        r#"{"t":"quote\" back\\slash \/slash"}"#,
        r#"{"t":"tab\t nl\n ctrl\u0001"}"#,
        r#"{"t":"unicode \u00e9 caf\u00e9 \u4e2d\u6587 \ud83d\ude00"}"#,
        r#"{"t":"emoji 😀 direct"}"#,
        // numbers: canonical stays verbatim, others go through serde/ryu
        r#"{"n":0,"neg":-42,"big":123456789012345678901234567890}"#,
        r#"{"f":1.10,"e":1e3,"neg0":-0,"exp":2.5e-7,"z":0.0}"#,
        // nesting + booleans + null
        r#"{"a":{"b":{"c":[{"d":1},true,false,null]}}}"#,
        // key containing a colon/comma/space
        r#"{"we:ird, key":"v"}"#,
        // string values that look like structure
        r#"{"t":"}{[,:\"","n":"12"}"#,
    ];
    for c in cases {
        assert_parity(c, "NEW_ID", "ses_X", Some("msg_Y"));
        assert_parity(c, "NEW_ID", "ses_X", None);
    }
}

#[test]
fn splice_matches_dom_with_unicode_and_control_char_ids() {
    // ids are ascii, but prove the escaping helper anyway via a stored value
    let src = r#"{"a":1}"#;
    assert_parity(src, "id\"with\\quote", "ses\u{00e9}", Some("msg\u{4e2d}"));
}

#[test]
fn splice_refuses_duplicate_keys_which_serde_collapses() {
    // serde's Value keeps the LAST duplicate (`{"k":1,"k":2}` -> `{"k":2}`),
    // so emitting both would change the wire. The splice refuses instead:
    // the DOM path then decides, which is correct by construction.
    for src in [r#"{"k":1,"k":2}"#, r#"{"a":1,"id":"x","a":2}"#] {
        let mut out = Vec::new();
        let mut scratch = SpliceScratch::default();
        assert!(
            !compact_splice(src, "I", "S", Some("M"), &mut out, &mut scratch),
            "must refuse duplicate keys: {src}"
        );
    }
}

#[test]
fn splice_tolerates_trailing_comma_that_serde_rejects() {
    // serde rejects `{"a":1,}` so no DOM oracle exists for it. The splice
    // still emits valid JSON — strictly safer than the old behavior (which
    // errored the whole request), so the contract is "valid output", not
    // "byte-identical".
    let mut out = Vec::new();
    let mut scratch = SpliceScratch::default();
    assert!(compact_splice(
        r#"{"a":1,}"#,
        "I",
        "S",
        Some("M"),
        &mut out,
        &mut scratch
    ));
    let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["a"], 1);
    assert_eq!(v["id"], "I");
}

#[test]
fn splice_refuses_malformed_so_dom_path_runs() {
    let bad = [
        "not json",
        "[1,2,3]",    // not an object
        r#"{"a":1"#,  // unterminated
        r#"{"a":}"#,  // missing value
        r#"{"a" 1}"#, // missing colon
        r#"{,}"#,     // leading comma
        "",
        "{",
    ];
    for b in bad {
        let mut out = Vec::new();
        let mut scratch = SpliceScratch::default();
        assert!(
            !compact_splice(b, "i", "s", Some("m"), &mut out, &mut scratch),
            "must refuse {b:?} and let the caller fall back"
        );
    }
}

#[test]
fn splice_reuses_caller_buffer_without_growing_it() {
    // the caller owns `out` for the whole page; the splice must not
    // reallocate it per message
    let mut out = Vec::with_capacity(64 * 1024);
    let mut scratch = SpliceScratch::default();
    let ptr = out.as_ptr();
    for i in 0..1000 {
        let src = format!(r#"{{"type":"text","text":"message {i}","n":{i}}}"#);
        assert!(compact_splice(
            &src,
            "id",
            "ses",
            Some("m"),
            &mut out,
            &mut scratch
        ));
    }
    assert_eq!(
        ptr,
        out.as_ptr(),
        "buffer must be reused, not reallocated (perf invariant)"
    );
}

#[test]
fn splice_output_is_valid_json() {
    let src = r#"{"type":"text", "text":"a,b}c", "time":{"start":1}}"#;
    let mut out = Vec::new();
    let mut scratch = SpliceScratch::default();
    assert!(compact_splice(
        src,
        "i",
        "s",
        Some("m"),
        &mut out,
        &mut scratch
    ));
    let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["id"], "i");
    assert_eq!(v["sessionID"], "s");
    assert_eq!(v["messageID"], "m");
    assert_eq!(v["text"], "a,b}c");
}

/// **Corpus differential** — the actual ship gate. Runs only when
/// `REFINE_SPLICE_DB` is set; CI runs it manually against a real db (and the
/// pre-commit gate records the run in PERF-10X). Every stored `info` and
/// `inline` row must splice byte-identically to the DOM reference, and any
/// refusal must be counted (refusals are allowed but must be zero on real
/// data — a refusal means that row pays the DOM path, so we want to know).
#[test]
fn splice_parity_over_corpus() {
    let Ok(db) = std::env::var("REFINE_SPLICE_DB") else {
        eprintln!("skipped: REFINE_SPLICE_DB not set (corpus differential is run manually)");
        return;
    };
    let conn = refine_store::pragma::open_reader(std::path::Path::new(&db)).unwrap();
    let mut scratch = SpliceScratch::default();
    let mut checked = 0u64;
    let mut refused = 0u64;
    let mut mismatched = 0u64;
    let mut examples: Vec<String> = Vec::new();

    // every message info
    let mut st = conn
        .prepare("SELECT id, session_id, info FROM msg")
        .unwrap();
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap();
    for r in rows.flatten() {
        let (mid, sid, info) = r;
        let mut out = Vec::with_capacity(info.len() + 160);
        if !compact_splice(&info, &mid, &sid, None, &mut out, &mut scratch) {
            refused += 1;
            if examples.len() < 5 {
                examples.push(format!("REFUSED msg.info: {info}"));
            }
            continue;
        }
        let got = String::from_utf8(out).unwrap();
        let want = dom_reference(&info, &mid, &sid, None);
        if got != want {
            mismatched += 1;
            if examples.len() < 5 {
                examples.push(format!(
                    "MISMATCH msg.info\n  src: {info}\n  got: {got}\n want: {want}"
                ));
            }
        }
        checked += 1;
    }

    // every part inline
    let mut st = conn
        .prepare("SELECT id, session_id, message_id, inline FROM msg_part WHERE inline IS NOT NULL")
        .unwrap();
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .unwrap();
    for r in rows.flatten() {
        let (pid, sid, mid, inline) = r;
        let mut out = Vec::with_capacity(inline.len() + 160);
        if !compact_splice(&inline, &pid, &sid, Some(&mid), &mut out, &mut scratch) {
            refused += 1;
            if examples.len() < 5 {
                examples.push(format!("REFUSED inline: {inline}"));
            }
            continue;
        }
        let got = String::from_utf8(out).unwrap();
        let want = dom_reference(&inline, &pid, &sid, Some(&mid));
        if got != want {
            mismatched += 1;
            if examples.len() < 5 {
                examples.push(format!(
                    "MISMATCH inline\n  src: {inline}\n  got: {got}\n want: {want}"
                ));
            }
        }
        checked += 1;
    }

    eprintln!("splice corpus: checked={checked} refused={refused} mismatched={mismatched}");
    for e in &examples {
        eprintln!("{e}");
    }
    assert_eq!(
        mismatched, 0,
        "splice must be byte-identical to the DOM path"
    );
    assert_eq!(refused, 0, "splice must not refuse any stored row");
}
