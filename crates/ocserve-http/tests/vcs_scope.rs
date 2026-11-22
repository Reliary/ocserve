//! K-VCS-SCOPE — `/api/vcs*` read routes answer for the REQUEST's directory.
//!
//! `GET /api/vcs`, `/status`, `/diff`, `/base` and `/branch` read the server's
//! working directory and echoed it as `location.directory`, whatever
//! `location[directory]` or `x-opencode-directory` asked for. A server started
//! outside a repository therefore reported "no branch, no changes" for every
//! project. The test process cwd is the ocserve checkout (itself a git repo),
//! so answering for the wrong directory is observable as the wrong location.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::AppState;
use serde_json::Value;
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
async fn location_query_scopes_info_status_base_and_branch() {
    let project = repo();
    std::fs::write(project.path().join("new.txt"), "x").unwrap();
    let p = project.path().to_str().unwrap();
    let want = canon(p);
    // Guard against a vacuous pass: the server cwd must differ from the project.
    assert_ne!(canon(std::env::current_dir().unwrap()), want);
    let current = git(project.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
    for route in [
        "/api/vcs",
        "/api/vcs/status",
        "/api/vcs/base",
        "/api/vcs/branch",
    ] {
        let uri = format!("{route}?location[directory]={p}");
        let (st, v) = call(&app(), "GET", &uri, &[], None).await;
        assert_eq!(st, StatusCode::OK, "{uri}: {v}");
        assert_eq!(
            canon(v["location"]["directory"].as_str().unwrap()),
            want,
            "{uri} must echo the requested directory"
        );
        match route {
            "/api/vcs" => assert_eq!(v["data"]["branch"]["current"], current, "{v}"),
            "/api/vcs/status" => assert!(
                v["data"].to_string().contains("new.txt"),
                "untracked file of the requested project must be listed: {v}"
            ),
            "/api/vcs/branch" => assert!(v["data"].to_string().contains(&current), "{v}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn header_scopes_the_request_and_is_percent_decoded() {
    let project = repo();
    let p = project.path().to_str().unwrap();
    let want = canon(p);
    let encoded = p.replace('/', "%2F");
    for value in [p.to_string(), encoded] {
        let (st, v) = call(
            &app(),
            "GET",
            "/api/vcs",
            &[("x-opencode-directory", value.as_str())],
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{value}: {v}");
        assert_eq!(
            canon(v["location"]["directory"].as_str().unwrap()),
            want,
            "{value}"
        );
    }
}

#[tokio::test]
async fn diff_is_scoped_to_the_requested_directory() {
    let project = repo();
    std::fs::write(project.path().join("tracked.txt"), "one\n").unwrap();
    git(project.path(), &["add", "tracked.txt"]);
    git(project.path(), &["commit", "-q", "-m", "add"]);
    std::fs::write(project.path().join("tracked.txt"), "two\n").unwrap();
    let p = project.path().to_str().unwrap();
    let uri = format!("/api/vcs/diff?mode=git&location[directory]={p}");
    let (st, v) = call(&app(), "GET", &uri, &[], None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["data"].to_string().contains("tracked.txt"), "{v}");
}

#[tokio::test]
async fn unscoped_requests_keep_answering_for_the_server_directory() {
    let (st, v) = call(&app(), "GET", "/api/vcs", &[], None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let server = std::env::current_dir().unwrap();
    assert_eq!(
        canon(v["location"]["directory"].as_str().unwrap()),
        canon(server)
    );
}

#[tokio::test]
async fn init_honours_the_location_query_the_header_and_the_body() {
    let by_query = tempfile::tempdir().unwrap();
    let by_header = tempfile::tempdir().unwrap();
    let by_body = tempfile::tempdir().unwrap();
    let q = by_query.path().to_str().unwrap();
    let h = by_header.path().to_str().unwrap();
    let b = by_body.path().to_str().unwrap();

    let (st, v) = call(
        &app(),
        "POST",
        &format!("/api/vcs/init?location[directory]={q}"),
        &[],
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{v}");
    let (st, v) = call(
        &app(),
        "POST",
        "/api/vcs/init",
        &[("x-opencode-directory", h)],
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{v}");
    let (st, v) = call(
        &app(),
        "POST",
        "/api/vcs/init",
        &[],
        Some(serde_json::json!({"directory": b})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{v}");

    for d in [&by_query, &by_header, &by_body] {
        assert!(
            d.path().join(".git").exists(),
            "{} must have been initialised",
            d.path().display()
        );
    }
}

#[tokio::test]
async fn init_accepts_an_empty_json_body_and_rejects_malformed_json() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap();
    // Empty body with a JSON content type (what SDKs send): scoped by query.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/vcs/init?location[directory]={d}"))
        .header("content-type", "application/json")
        .body(Body::empty())
        .unwrap();
    let resp = app().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(dir.path().join(".git").exists());

    // Malformed JSON must stay a 400, never fall through to the server cwd.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/vcs/init?location[directory]={d}"))
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let resp = app().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn init_refuses_a_relative_or_dash_body_directory() {
    // A relative path would resolve against the server cwd, and a leading dash
    // must never reach git as an option. Both are refused; nothing is created.
    let before = std::env::current_dir().unwrap();
    for d in ["--bare", "relative/dir"] {
        let (st, v) = call(
            &app(),
            "POST",
            "/api/vcs/init",
            &[],
            Some(serde_json::json!({"directory": d})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{d}: {v}");
        assert!(!before.join(d).exists(), "{d} must not have been created");
    }
}

#[tokio::test]
async fn init_body_directory_wins_over_the_query() {
    let by_body = tempfile::tempdir().unwrap();
    let by_query = tempfile::tempdir().unwrap();
    let (st, v) = call(
        &app(),
        "POST",
        &format!(
            "/api/vcs/init?location[directory]={}",
            by_query.path().display()
        ),
        &[],
        Some(serde_json::json!({"directory": by_body.path().to_str().unwrap()})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{v}");
    assert!(by_body.path().join(".git").exists());
    assert!(!by_query.path().join(".git").exists());
}
