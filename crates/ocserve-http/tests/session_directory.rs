//! K-SESSIONDIR — the "session thinks it's in the ocserve repo" + constant
//! `external_directory` permission-prompt class (2026-10-11).
//!
//! Root cause: `create_session_record` ignored the client's requested directory
//! and stamped every session with the SERVER's cwd (`st.paths["worktree"]`, the
//! ocserve checkout). The web UI creates a session for the user's project with
//! `?directory=` (v1) / `body.location.directory` (v2); ocserve dropped it, so
//! the prompt tool ops ran in the ocserve repo and every path in the user's
//! real project resolved OUTSIDE the (wrong) worktree → an
//! `external_directory` ask per file. Upstream scopes the session to the
//! requested directory: `directory` = git toplevel (or the dir itself),
//! `path` = dir relative to that root.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::AppState;
use serde_json::Value;
use tower::ServiceExt;

fn app() -> axum::Router {
    ocserve_http::router(AppState::new())
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get(app: &axum::Router, uri: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Canonical git init in a fresh temp dir; returns (dir, toplevel).
fn git_repo() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["init", "-q"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git init failed (git required for this suite)");
    let top = std::process::Command::new("git")
        .arg("-C")
        .arg(p)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .unwrap();
    let top = String::from_utf8_lossy(&top.stdout).trim().to_string();
    (dir, top)
}

#[tokio::test]
async fn v1_directory_query_is_honored() {
    let app = app();
    let (_t, repo) = git_repo();
    // a subdirectory of the repo → directory = repo toplevel, path = rel
    let sub = format!("{repo}/a/b");
    std::fs::create_dir_all(&sub).unwrap();
    let (st, info) = post(
        &app,
        &format!("/session?directory={sub}"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(info["directory"], repo, "directory = git toplevel");
    assert_eq!(info["path"], "a/b", "path = dir relative to toplevel");
    // and the served session must carry the same directory (the field the
    // prompt context + external_directory gate read)
    let sid = info["id"].as_str().unwrap();
    let listed = get(&app, "/session").await;
    let row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == sid)
        .expect("created session listed");
    assert_eq!(row["directory"], repo, "served session directory persisted");
}

#[tokio::test]
async fn non_repo_directory_uses_itself_as_root() {
    let app = app();
    // a temp dir that is NOT a git repo (its own root; path = leading-slash-stripped)
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_string_lossy().to_string();
    let (st, info) = post(
        &app,
        &format!("/session?directory={d}"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        info["directory"], d,
        "non-repo → directory is the dir itself"
    );
    assert_eq!(
        info["path"],
        d.trim_start_matches('/'),
        "non-repo → path = dir without leading slash"
    );
}

#[tokio::test]
async fn v2_location_directory_is_honored() {
    let app = app();
    let (_t, repo) = git_repo();
    let (st, body) = post(
        &app,
        "/api/session",
        serde_json::json!({"location": {"directory": repo}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["data"]["directory"], repo);
    assert_eq!(body["data"]["path"], "");
}

#[tokio::test]
async fn absent_directory_falls_back_to_server_worktree() {
    let app = app();
    let (st, info) = post(&app, "/session", serde_json::json!({})).await;
    assert_eq!(st, StatusCode::OK);
    // freeze default = the server's launch worktree (AppState derives it from
    // the process cwd's git toplevel); must be a real absolute path, never a
    // hardcoded "/" or empty.
    let dir = info["directory"].as_str().unwrap();
    assert!(dir.starts_with('/'), "absolute worktree: {dir}");
    let cwd_root = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string()
        });
    assert_eq!(dir, cwd_root, "fallback = server launch worktree");
}

#[tokio::test]
async fn project_list_includes_session_directories() {
    let app = app();
    let (_t, repo) = git_repo();
    post(
        &app,
        &format!("/session?directory={repo}"),
        serde_json::json!({}),
    )
    .await;
    let projects = get(&app, "/project").await;
    let arr = projects.as_array().unwrap();
    assert_eq!(arr[0]["id"], "global", "global always first");
    assert!(
        arr.iter().any(|p| p["worktree"] == repo),
        "the session's directory must appear as a project (web UI grouping): {projects}"
    );
}
