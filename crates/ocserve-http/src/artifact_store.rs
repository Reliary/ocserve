//! Local artifact store (v2 `experimental.artifacts`, upstream
//! `packages/core/src/artifact.ts` local provider).
//!
//! Kept OUT of the HTTP router: the router validates requests and shapes
//! responses, this module owns file placement. Containment is the shared
//! handle-based rule (`crate::containment`), never a string prefix — the
//! reason guard rule 43 bans `create_dir_all` on an unresolved path.

use serde_json::{Value, json};

/// `normalizePath` (upstream `artifact.ts:484`): backslashes → slashes, strip
/// leading `./` and `/`, reject empty/NUL/absolute/dot segments.
pub(crate) fn normalize_path(raw: &str) -> Result<String, crate::HttpError> {
    let mut t = raw.replace('\\', "/");
    while let Some(rest) = t.strip_prefix("./") {
        t = rest.to_string();
    }
    let t = t.trim_start_matches('/').to_string();
    if t.is_empty() || t.contains('\0') {
        return Err(crate::v2::artifact_invalid(
            &format!("Invalid artifact file path: {:?}", raw),
            "files",
        ));
    }
    if t.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        return Err(crate::v2::artifact_invalid(
            &format!("Artifact file path must stay relative: {:?}", raw),
            "files",
        ));
    }
    Ok(t)
}

/// Root of the local artifact store, under the server's data dir (upstream
/// keeps it in the global state dir; ours is the same data dir as the DB).
pub(crate) fn root(st: &crate::AppState) -> std::path::PathBuf {
    ocserve_store::platform::data_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("artifacts")
        .join(
            st.paths["directory"]
                .as_str()
                .unwrap_or("/")
                .trim_start_matches('/')
                .replace('/', "_"),
        )
}

/// Store an artifact's files on disk, atomically per file (tmp+rename).
pub(crate) fn write_files(
    root: &std::path::Path,
    id: &str,
    files: &serde_json::Map<String, Value>,
) -> anyhow::Result<()> {
    let dir = root.join(id);
    let _ = std::fs::remove_dir_all(&dir);
    for (rel, content) in files {
        let clean = normalize_path(rel).map_err(|_| anyhow::anyhow!("invalid artifact path"))?;
        // `artifact_normalize_path` already rejects absolute paths, `..` and
        // empty segments; re-verify through the handle-based rule so the
        // artifact writer shares ONE containment implementation (rule 43).
        let path = crate::v2::resolve_within(&dir, &clean)
            .map_err(|e| anyhow::anyhow!("invalid artifact path: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = content.as_str().unwrap_or("").as_bytes();
        let tmp = path.with_extension("ocserve-tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// The stored artifact record (one JSON file beside the files directory).
pub(crate) fn record_path(root: &std::path::Path, id: &str) -> std::path::PathBuf {
    root.join(format!("{id}.json"))
}

pub(crate) fn read_record(root: &std::path::Path, id: &str) -> Option<Value> {
    std::fs::read_to_string(record_path(root, id))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

pub(crate) fn list_records(root: &std::path::Path) -> Vec<Value> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("json")
                && let Ok(s) = std::fs::read_to_string(e.path())
                && let Ok(v) = serde_json::from_str::<Value>(&s)
            {
                out.push(v);
            }
        }
    }
    // Upstream sorts by updated desc, then id desc.
    out.sort_by(|a, b| {
        b["time"]["updated"]
            .as_i64()
            .cmp(&a["time"]["updated"].as_i64())
            .then_with(|| b["id"].as_str().cmp(&a["id"].as_str()))
    });
    out
}

/// `resolveEntry` (upstream): explicit entry must exist; else index.html,
/// index.md, a single file, else the "specify entry" error.
pub(crate) fn resolve_entry(
    files: &serde_json::Map<String, Value>,
    requested: Option<&str>,
) -> Result<String, crate::HttpError> {
    if let Some(r) = requested {
        let clean = normalize_path(r)?;
        if files.contains_key(&clean) {
            return Ok(clean);
        }
        return Err(crate::v2::artifact_invalid(
            &format!("Entry file not found in files: {clean}"),
            "entry",
        ));
    }
    if files.contains_key("index.html") {
        return Ok("index.html".into());
    }
    if files.contains_key("index.md") {
        return Ok("index.md".into());
    }
    if files.len() == 1
        && let Some(k) = files.keys().next()
    {
        return Ok(k.clone());
    }
    Err(crate::v2::artifact_invalid(
        "Include \"index.html\" or \"index.md\", or specify \"entry\" when publishing multiple files",
        "entry",
    ))
}

/// Normalise a `{path: string}` file map: at least one file, every path valid.
pub(crate) fn files(v: Option<&Value>) -> Result<serde_json::Map<String, Value>, crate::HttpError> {
    use axum::http::StatusCode;
    let Some(obj) = v.and_then(|v| v.as_object()) else {
        return Err(crate::HttpError::TaggedData {
            status: StatusCode::BAD_REQUEST,
            tag: "InvalidRequestError",
            fields: json!({"message": "Missing key\n  at [\"files\"]", "kind": "Payload"}),
        });
    };
    if obj.is_empty() {
        return Err(crate::v2::artifact_invalid(
            "Artifact must include at least one file",
            "files",
        ));
    }
    let mut out = serde_json::Map::new();
    for (k, content) in obj {
        let clean = normalize_path(k)?;
        let s = content
            .as_str()
            .ok_or_else(|| crate::HttpError::TaggedData {
                status: StatusCode::BAD_REQUEST,
                tag: "InvalidRequestError",
                fields: json!({"message": format!("Expected a string\n  at [\"files\"][\"{k}\"]"), "kind": "Payload"}),
            })?;
        out.insert(clean, json!(s));
    }
    Ok(out)
}

/// Create the artifact store root. The root is server-owned (under the data
/// dir), so a plain recursive create is correct here — there is no
/// user-controlled component — but it lives in the store module so the router
/// never performs file placement (rule 43).
pub(crate) fn ensure_root(root: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)
}
