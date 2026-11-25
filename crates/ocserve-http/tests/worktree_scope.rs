//! K-WORKTREE-SCOPE — worktree routes act on the REQUEST's project, not the
//! server cwd.
//!
//! `POST /api/worktree` took `projectID` and discarded it, then ran
//! `git rev-parse` in the server's own working directory. A server started
//! outside a repository answered `WorktreeNotGitError` for every git project,
//! and one started inside a repository silently cut worktrees of THAT
//! repository instead of the project's. These tests run with the test
//! process cwd inside the ocserve checkout (a git repo) so the second failure
//! is observable: the new worktree must belong to the requested project.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::AppState;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use tower::ServiceExt;

fn app() -> axum::Router {
    ocserve_http::router(AppState::with_payloads_and_v2(
        None,
        ocserve_http::Payloads::default(),
        true,
    ))
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let st = resp.status();
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    git(d.path(), &["init", "-q"]);
    git(d.path(), &["commit", "-q", "--allow-empty", "-m", "init"]);
    d
}

fn canon(p: impl AsRef<Path>) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap()
}

#[tokio::test]
async fn create_cuts_the_worktree_from_the_requested_project() {
    let project = repo();
    let scratch = tempfile::tempdir().unwrap();
    let target = scratch.path().join("wt");
    let (st, v) = call(
        &app(),
        "POST",
        "/api/worktree",
        &[],
        Some(json!({
            "projectID": project.path().to_str().unwrap(),
            "directory": target.to_str().unwrap(),
            "branch": "HEAD",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "body {v}");
    let common = git(&target, &["rev-parse", "--git-common-dir"]);
    assert_eq!(
        canon(&common),
        canon(project.path().join(".git")),
        "the worktree must belong to the requested project, not the server cwd"
    );
}

#[tokio::test]
async fn create_on_a_non_git_project_is_worktree_not_git() {
    let plain = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (st, v) = call(
        &app(),
        "POST",
        "/api/worktree",
        &[],
        Some(json!({
            "projectID": plain.path().to_str().unwrap(),
            "directory": scratch.path().join("wt").to_str().unwrap(),
        })),
    )
    .await;
    // The server cwd is a git repo here; refusing proves the PROJECT was checked.
    assert_eq!(st, StatusCode::BAD_REQUEST, "body {v}");
    assert_eq!(v["name"], "WorktreeNotGitError");
}

#[tokio::test]
async fn header_and_location_query_scope_the_request() {
    let project = repo();
    let p = project.path().to_str().unwrap().to_string();
    for (uri, headers) in [
        (format!("/api/worktree?location[directory]={p}"), vec![]),
        (
            "/api/worktree".to_string(),
            vec![("x-opencode-directory", p.as_str())],
        ),
    ] {
        let (st, v) = call(&app(), "GET", &uri, &headers, None).await;
        assert_eq!(st, StatusCode::OK, "{uri}: {v}");
        let dirs: Vec<_> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|e| canon(e["directory"].as_str().unwrap()))
            .collect();
        assert_eq!(dirs, vec![canon(project.path())], "{uri}");
    }
}

#[tokio::test]
async fn remove_acts_on_the_requested_project() {
    let project = repo();
    let scratch = tempfile::tempdir().unwrap();
    let target = scratch.path().join("wt");
    git(
        project.path(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            target.to_str().unwrap(),
        ],
    );
    let (st, v) = call(
        &app(),
        "DELETE",
        "/api/worktree",
        &[],
        Some(json!({
            "projectID": project.path().to_str().unwrap(),
            "directory": target.to_str().unwrap(),
            "force": true,
        })),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "body {v}");
    assert!(!target.exists(), "worktree directory must be gone");
}

#[tokio::test]
async fn an_explicit_project_outranks_the_ambient_session_header() {
    let project = repo();
    let other = repo();
    let scratch = tempfile::tempdir().unwrap();
    let target = scratch.path().join("wt");
    let (st, v) = call(
        &app(),
        "POST",
        "/api/worktree",
        &[("x-opencode-directory", other.path().to_str().unwrap())],
        Some(json!({
            "projectID": project.path().to_str().unwrap(),
            "directory": target.to_str().unwrap(),
            "branch": "HEAD",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "body {v}");
    assert_eq!(
        canon(git(&target, &["rev-parse", "--git-common-dir"])),
        canon(project.path().join(".git")),
        "the body's projectID names the project, not the header"
    );
}

#[tokio::test]
async fn percent_encoded_header_and_directory_query_are_honoured() {
    let parent = tempfile::tempdir().unwrap();
    let spaced = parent.path().join("my project");
    std::fs::create_dir(&spaced).unwrap();
    git(&spaced, &["init", "-q"]);
    git(&spaced, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let encoded = spaced.to_str().unwrap().replace(' ', "%20");

    for (uri, headers) in [
        (
            "/api/worktree".to_string(),
            vec![("x-opencode-directory", encoded.as_str())],
        ),
        (format!("/api/worktree?directory={encoded}"), vec![]),
    ] {
        let (st, v) = call(&app(), "GET", &uri, &headers, None).await;
        assert_eq!(st, StatusCode::OK, "{uri}: {v}");
        let dirs: Vec<_> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|e| canon(e["directory"].as_str().unwrap()))
            .collect();
        assert_eq!(dirs, vec![canon(&spaced)], "{uri}");
    }
}

#[tokio::test]
async fn relative_scope_values_fall_through_to_the_server_directory() {
    // A relative value must not silently resolve against the server cwd.
    let (st, v) = call(
        &app(),
        "GET",
        "/api/worktree?location[directory]=relative/path",
        &[("x-opencode-directory", "also/relative")],
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // The list names repo roots; the server cwd is somewhere inside one.
    let server = canon(git(
        &std::env::current_dir().unwrap(),
        &["rev-parse", "--show-toplevel"],
    ));
    let dirs: Vec<_> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let d = e["directory"].as_str().unwrap();
            std::fs::canonicalize(d).unwrap_or_else(|_| d.into())
        })
        .collect();
    assert!(
        dirs.contains(&server),
        "falls through to the server cwd: {dirs:?}"
    );
}

#[tokio::test]
async fn a_dash_prefixed_branch_is_refused_not_parsed_as_a_git_option() {
    let project = repo();
    let scratch = tempfile::tempdir().unwrap();
    let (st, v) = call(
        &app(),
        "POST",
        "/api/worktree",
        &[],
        Some(json!({
            "projectID": project.path().to_str().unwrap(),
            "directory": scratch.path().join("wt").to_str().unwrap(),
            "branch": "--orphan",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "body {v}");
    assert!(!scratch.path().join("wt").exists());
}
