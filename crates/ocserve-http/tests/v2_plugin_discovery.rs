//! K-V2-LAUNCHER: a plugin package provisioned under
//! `<config_dir>/plugins/<name>/` after boot is discovered, loaded, and
//! reported through the CONTRACT's `Plugin.Info` shape with
//! `state.status == "active"`.
//!
//! This is exactly what Paseo's v2 adapter does before it binds its bridge:
//! provision the package directory, then poll `GET /api/plugin` until the
//! plugin is active. A test that only checked `discover_plugin_specs` would
//! pass while the inventory poll still returned the old private
//! `{state: "loaded"}` string that no launcher can read.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, Payloads};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

async fn state(_dir: &std::path::Path) -> Arc<AppState> {
    AppState::with_payloads_and_v2(None, Payloads::default(), true)
}

async fn plugin_list(st: &Arc<AppState>) -> Vec<Value> {
    let resp = ocserve_http::router(st.clone())
        .oneshot(
            Request::builder()
                .uri("/api/plugin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    v["data"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn discovered_v2_plugin_is_active_in_the_contract_inventory() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let st = state(dir.path()).await;

    // Paseo's provisioning layout: plugins/<package>/{package.json,server.js}.
    let cfg_dir = dir.path().join("cfg");
    let pkg = cfg_dir.join("plugins/discovered");
    std::fs::create_dir_all(&pkg)?;
    std::fs::write(
        pkg.join("package.json"),
        r#"{"name":"discovered","private":true,"type":"module","exports":{"./server":"./server.js"}}"#,
    )?;
    std::fs::write(
        pkg.join("server.js"),
        r#"export default {
  id: "discovered",
  setup: async (ctx) => { await ctx.session.hook("context", () => {}); },
};"#,
    )?;
    // The poll path. Before the entry existed this was a no-op; now it loads.
    ocserve_http::v2::load_discovered_from(&st, &cfg_dir).await;
    let data = plugin_list(&st).await;
    let found: Vec<&Value> = data
        .iter()
        .filter(|p| {
            p["source"]["path"]
                .as_str()
                .is_some_and(|s| s.ends_with("/plugins/discovered"))
        })
        .collect();
    assert_eq!(found.len(), 1, "one discovered plugin: {data:?}");
    let p = found[0];
    assert_eq!(p["id"], "discovered");
    assert_eq!(
        p["source"]["type"], "local",
        "a directory spec is a local source: {p}"
    );
    assert_eq!(p["features"]["server"], true, "{p}");
    assert_eq!(
        p["state"]["status"], "active",
        "Paseo polls exactly this field: {p}"
    );

    // The 50 ms poll is idempotent: a second pass must not load/list it twice.
    ocserve_http::v2::load_discovered_from(&st, &cfg_dir).await;
    let again = plugin_list(&st).await;
    let found: Vec<&Value> = again
        .iter()
        .filter(|p| {
            p["source"]["path"]
                .as_str()
                .is_some_and(|s| s.ends_with("/plugins/discovered"))
        })
        .collect();
    assert_eq!(found.len(), 1, "{again:?}");
    Ok(())
}

#[tokio::test]
async fn failed_discovery_reports_failed_instead_of_absent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let st = state(dir.path()).await;
    // A directory with no entrypoint: discovery finds it, load fails, and the
    // inventory must say `failed` (with the error) — never omit it.
    let cfg_dir = dir.path().join("cfg");
    std::fs::create_dir_all(cfg_dir.join("plugins/broken"))?;
    ocserve_http::v2::load_discovered_from(&st, &cfg_dir).await;
    let data = plugin_list(&st).await;
    let found: Vec<&Value> = data
        .iter()
        .filter(|p| {
            p["source"]["path"]
                .as_str()
                .is_some_and(|s| s.ends_with("/plugins/broken"))
        })
        .collect();
    assert_eq!(found.len(), 1, "{data:?}");
    assert_eq!(found[0]["state"]["status"], "failed", "{:?}", found[0]);
    assert!(
        found[0]["state"]["error"]
            .as_str()
            .is_some_and(|e| !e.is_empty()),
        "{:?}",
        found[0]
    );
    Ok(())
}
