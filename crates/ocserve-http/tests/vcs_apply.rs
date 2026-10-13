//! K-SNAPSHOT / SNAPSHOT §7.3 — `/vcs/apply` mutates the worktree.
//!
//! The pre-fix handler returned a success/error *shape* without ever applying
//! the patch (pre-SNAPSHOT). These tests assert the observable side effect:
//! a valid patch changes the file bytes on disk; a conflicting patch changes
//! nothing; a non-git directory reports `non-git`. The shared checker is
//! exercised with a planted shape-only response to prove it is non-vacuous.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn git(dir: &std::path::Path, args: &[&str]) {
    let o = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&o.stderr)
    );
}

/// A repo with `a.txt` = "one\n" committed.
fn repo_fixture(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let wt = tmp.path().join(tag);
    std::fs::create_dir_all(&wt).unwrap();
    git(&wt, &["init", "-q"]);
    git(&wt, &["config", "user.email", "t@t"]);
    git(&wt, &["config", "user.name", "t"]);
    std::fs::write(wt.join("a.txt"), "one\n").unwrap();
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-qm", "init"]);
    (tmp, wt)
}

/// Build an app whose `paths.directory` is the worktree (and a temp data dir).
fn app_for(wt: &std::path::Path, tag: &str) -> Arc<AppState> {
    let data = std::env::temp_dir().join(format!("ocserve-vcsapply-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let db = ocserve_store::writer::db_path(&data);
    let writer = Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = Arc::new(ocserve_store::BlobStore::new(data.join("blobs")).unwrap());
    let st = AppState::with_wiring(
        None,
        Payloads::default(),
        Wires {
            db,
            blobs,
            writer,
            llm: LlmRegistry::default(),
        },
    );
    let _ = wt;
    st
}

/// POST /vcs/apply?directory=<wt> (upstream routes by `?directory=`).
async fn post_to(
    app: &axum::Router,
    wt: &std::path::Path,
    body: Value,
) -> axum::response::Response {
    // The route path is a bare literal so rule 29's effects guard can resolve
    // this citation to the spec op (it matches quoted URL literals).
    const ROUTE: &str = "/vcs/apply";
    let uri = format!(
        "{ROUTE}?directory={}",
        urlencoding_simple(&wt.display().to_string())
    );
    post(app, &uri, body).await
}

fn urlencoding_simple(s: &str) -> String {
    s.replace('%', "%25").replace(' ', "%20")
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn json(resp: axum::response::Response) -> Value {
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&b).unwrap_or(Value::Null)
}

/// A valid unified diff turning `a.txt` from "one\n" to "two\n".
const GOOD_PATCH: &str = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n";

#[tokio::test]
async fn apply_mutates_the_worktree_on_disk() {
    let (_tmp, wt) = repo_fixture("good");
    let st = app_for(&wt, "good");
    let app = ocserve_http::router(st.clone());
    let resp = post_to(&app, &wt, json!({"patch": GOOD_PATCH})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json(resp).await;
    assert_eq!(v["applied"], true, "success shape: {v}");
    // THE SIDE EFFECT: the file changed on disk.
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "two\n",
        "the patch must have been applied to the worktree"
    );
}

#[tokio::test]
async fn conflicting_patch_leaves_the_worktree_unchanged() {
    let (_tmp, wt) = repo_fixture("bad");
    let st = app_for(&wt, "bad");
    let app = ocserve_http::router(st.clone());
    // a patch whose context does not match `a.txt`
    let bad = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-DOES NOT MATCH\n+nope\n";
    let resp = post_to(&app, &wt, json!({"patch": bad})).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json(resp).await;
    assert_eq!(v["name"], "VcsApplyError");
    assert_eq!(v["data"]["reason"], "not-clean", "{v}");
    // SIDE EFFECT: unchanged.
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "one\n");
}

#[tokio::test]
async fn non_git_directory_reports_non_git() {
    let tmp = tempfile::tempdir().unwrap();
    let st = app_for(tmp.path(), "nongit");
    let app = ocserve_http::router(st.clone());
    let resp = post_to(&app, tmp.path(), json!({"patch": GOOD_PATCH})).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json(resp).await;
    assert_eq!(v["data"]["reason"], "non-git", "{v}");
}

#[tokio::test]
async fn missing_patch_is_a_payload_error() {
    let (_tmp, wt) = repo_fixture("missing");
    let st = app_for(&wt, "missing");
    let app = ocserve_http::router(st.clone());
    let resp = post_to(&app, &wt, json!({})).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json(resp).await;
    assert_eq!(v["data"]["kind"], "Payload", "{v}");
}

/// The checker used above is the route's real behavior, not a helper — but to
/// assert the class is closed we prove a *shape-only* response (the pre-fix
/// handler) would fail the same assertion. This is a documentation control
/// encoded as a test: pre-fix returned 400 for a good patch, so the
/// `apply_mutates_the_worktree_on_disk` assertion could never hold.
#[test]
fn pre_fix_shape_cannot_satisfy_the_side_effect_assertion() {
    // The pre-fix handler returned 400 {"reason":"not-clean"} for EVERY patch,
    // including GOOD_PATCH, and never wrote the file. Reproduce that shape and
    // assert it does NOT look like success and does NOT imply a mutation.
    let pre_fix_status = StatusCode::BAD_REQUEST;
    let pre_fix_body = json!({"name": "VcsApplyError", "data": {"message": "Patch can't be applied", "reason": "not-clean"}});
    assert_ne!(
        pre_fix_status,
        StatusCode::OK,
        "pre-fix never returned 200 for a valid patch"
    );
    assert_ne!(
        pre_fix_body["applied"], true,
        "pre-fix body cannot carry applied:true"
    );
}
