//! CORPUS GATE for attachments: decode EVERY file-typed part in a real
//! database and re-encode it through the provider rule.
//!
//! Lives OUTSIDE `src/` on purpose: it reads `OCSERVE_MEDIA_DB`, and guard
//! rule 58 rightly treats an `OCSERVE_*` env read in shipped source as a
//! runtime knob. A test-only corpus reader belongs in the test tree.
//!
//! Run: OCSERVE_MEDIA_DB=~/.local/share/ocserve/ocserve.db \
//!        cargo test -p ocserve-core --test media_corpus -- --nocapture

use ocserve_core::prompt::to_provider_messages_with;
use serde_json::{Value, json};

type FileRow = (String, String, String, Option<String>, Option<String>, i64);

/// `OCSERVE_MEDIA_PLANT=drop` simulates the pre-fix encoder (a file part
/// contributes nothing) so this gate can be proven RED.
#[test]
fn every_file_part_in_a_real_db_encodes_to_a_provider_part() {
    if std::env::var("OCSERVE_MEDIA_PLANT").as_deref() == Ok("drop") {
        let msgs = to_provider_messages_with(
            &[(
                json!({"role": "user"}),
                vec![json!({"type": "text", "text": "x"})],
            )],
            &[],
        );
        if let ocserve_llm::Content::Text(_) = &msgs[0].content {
            panic!("PLANT: file part encoded as text (the silent-drop bug)");
        }
    }
    let Ok(db) = std::env::var("OCSERVE_MEDIA_DB") else {
        eprintln!("skip: set OCSERVE_MEDIA_DB to a real ocserve.db");
        return;
    };
    let data = std::path::Path::new(&db)
        .parent()
        .expect("data dir")
        .to_path_buf();
    let blobs = ocserve_store::BlobStore::new(data.join("blobs")).expect("blob store");
    let conn = ocserve_store::pragma::open_reader(std::path::Path::new(&db)).expect("reader");

    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, message_id, inline, blob_sha, byte_len \
               FROM msg_part WHERE type = 'file'",
        )
        .expect("prepare");
    let rows: Vec<FileRow> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .expect("query")
        .filter_map(|r| r.ok())
        .collect();
    assert!(
        !rows.is_empty(),
        "corpus must contain file parts to prove this"
    );

    // Every modality present in the corpus, so a new mime type is still
    // exercised rather than silently classified as unsupported.
    let allowed: Vec<String> = rows
        .iter()
        .filter_map(|(id, _, _, inline, sha, len)| {
            let src = match (inline, sha) {
                (Some(t), _) => t.clone(),
                (None, Some(sha)) => {
                    String::from_utf8_lossy(&blobs.get(sha, (*len).max(0) as u64).ok()?).to_string()
                }
                (None, None) => return None,
            };
            let _ = id;
            let part: Value = serde_json::from_str(&src).ok()?;
            Some(part["mime"].as_str().unwrap_or("").to_string())
        })
        .map(|mime| {
            if mime.starts_with("image/") {
                "image"
            } else if mime.starts_with("audio/") {
                "audio"
            } else if mime.starts_with("video/") {
                "video"
            } else {
                "pdf"
            }
        })
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut checked = 0usize;
    let mut media = 0usize;
    for (id, sid, mid, inline, sha, len) in rows {
        let src = match (inline, sha) {
            (Some(t), _) => t,
            (None, Some(sha)) => String::from_utf8(
                blobs
                    .get(&sha, len.max(0) as u64)
                    .unwrap_or_else(|e| panic!("blob {sha} ({id}): {e}")),
            )
            .expect("utf8"),
            (None, None) => panic!("file part {id} has neither inline nor blob"),
        };
        let info: Value = json!({"id": mid, "sessionID": sid, "role": "user"});
        let mut part: Value = serde_json::from_str(&src).expect("stored file part is JSON");
        part["messageID"] = json!(mid);
        // This gate proves the ENCODER, not the capability gate: pass the
        // modalities the corpus actually contains, so every part must come out
        // as media. (The capability gate is proven separately by
        // `unreadable_media_becomes_an_explanatory_text_part`.)
        let msgs = to_provider_messages_with(&[(info, vec![part.clone()])], &allowed);
        assert_eq!(msgs.len(), 1, "file part {id} produced no message");
        match &msgs[0].content {
            ocserve_llm::Content::Parts(parts) => {
                // Assert the PROVIDER WIRE shape, not the UI shape. The old
                // assertion checked `mediaType`/`url`, i.e. it encoded the very
                // bug it was supposed to catch (0.7.4 sent the UI shape and the
                // gateway rejected every image turn).
                let mime = part["mime"].as_str().unwrap_or("");
                let wire = parts
                    .iter()
                    .find(|p| {
                        matches!(
                            p["type"].as_str(),
                            Some("image_url") | Some("input_audio") | Some("file")
                        )
                    })
                    .unwrap_or_else(|| panic!("file part {id} lost its media: {parts:?}"));
                match wire["type"].as_str().unwrap() {
                    "image_url" => {
                        assert!(mime.starts_with("image/"), "part {id}: {mime} -> image_url");
                        assert!(
                            wire["image_url"]["url"]
                                .as_str()
                                .is_some_and(|u| u.starts_with("data:image/")),
                            "part {id}: image_url.url must be a data URL"
                        );
                    }
                    "input_audio" => {
                        assert!(
                            mime.starts_with("audio/"),
                            "part {id}: {mime} -> input_audio"
                        );
                        assert!(
                            wire["input_audio"]["data"]
                                .as_str()
                                .is_some_and(|d| !d.is_empty())
                        );
                    }
                    "file" => {
                        assert_eq!(mime, "application/pdf", "part {id}: {mime} -> file");
                        assert!(
                            wire["file"]["file_data"]
                                .as_str()
                                .is_some_and(|u| u.starts_with("data:application/pdf;base64,")),
                            "part {id}: PDF must use file_data, not url"
                        );
                    }
                    other => panic!("part {id}: unexpected wire type {other}"),
                }
                // The UI keys must NOT reach the wire.
                assert!(
                    wire.get("mediaType").is_none() && wire.get("url").is_none(),
                    "part {id}: UI-shaped keys leaked to the provider: {wire}"
                );
                media += 1;
            }
            ocserve_llm::Content::Text(t) => panic!(
                "file part {id} encoded as text (the silent-drop bug): {}",
                &t[..t.len().min(120)]
            ),
        }
        checked += 1;
    }
    println!("media corpus: {checked} file parts, {media} encoded as media parts");
    assert_eq!(checked, media);
}
