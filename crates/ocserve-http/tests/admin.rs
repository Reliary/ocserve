//! W5 admin routes — source-derived contracts: MCP connect/disconnect +
//! authRemove, auth overlay CRUD, provider auth methods, global dispose.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use tower::ServiceExt;

const FAKE_MCP: &str = r#"#!/usr/bin/env python3
import sys, json
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("method") == "initialize":
        resp = {"jsonrpc":"2.0","id":msg["id"],"result":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "serverInfo":{"name":"fake","version":"0"}}}
    elif msg.get("method") == "notifications/initialized":
        continue
    else:
        resp = {"jsonrpc":"2.0","id":msg.get("id"),"result":{}}
    sys.stdout.write(json.dumps(resp)+"\n"); sys.stdout.flush()
"#;

fn state(dir: &std::path::Path, payloads: Payloads) -> std::sync::Arc<AppState> {
    let db = ocserve_store::writer::db_path(dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [("fake".into(), ("http://127.0.0.1:9".into(), String::new()))]
            .into_iter()
            .collect(),
        pricing: Default::default(),
        default_model: ("fake".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    };
    AppState::with_wiring(
        None,
        payloads,
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
    )
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, Vec<u8>) {
    let mut b = Request::builder().method(method).uri(uri);
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let resp = app
        .clone()
        .oneshot(
            b.body(Body::from(body.unwrap_or_default().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes)
}

#[tokio::test]
async fn mcp_connect_disconnect_and_not_found_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path(), Payloads::default());
    // fake MCP server (initialize handshake only)
    let script = dir.path().join("fake_mcp.py");
    std::fs::write(&script, FAKE_MCP).unwrap();
    let hub = ocserve_mcp::McpHub::probe_all(&[ocserve_mcp::ServerCfg {
        name: "fakey".into(),
        enabled: true,
        kind: ocserve_mcp::Kind::Local {
            command: vec!["python3".into(), script.to_string_lossy().into()],
            env: vec![],
        },
    }])
    .await;
    assert_eq!(
        hub.statuses()["fakey"]["status"],
        "connected",
        "probe connected"
    );
    st.mcp.set(std::sync::Arc::new(hub)).ok();
    let app = ocserve_http::router(st.clone());

    // disconnect → true; status flips; connect → true again
    let (s, b) = call(&app, "POST", "/mcp/fakey/disconnect", None).await;
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
    assert_eq!(std::str::from_utf8(&b).unwrap(), "true");
    assert_eq!(
        st.mcp.get().unwrap().statuses()["fakey"]["status"],
        "disconnected"
    );
    let (s, _) = call(&app, "POST", "/mcp/fakey/connect", None).await;
    assert_eq!(s, 200);
    assert_eq!(
        st.mcp.get().unwrap().statuses()["fakey"]["status"],
        "connected"
    );

    // unknown server → 404 TaggedErrorClass flat envelope (errors.ts:143)
    let (s, b) = call(&app, "POST", "/mcp/nope/connect", None).await;
    assert_eq!(s, 404);
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["_tag"], "McpServerNotFoundError");
    assert_eq!(v["name"], "nope");
    assert_eq!(v["message"], "MCP server not found: nope");

    // unknown action → 500 (client only ever sends connect/disconnect)
    let (s, _) = call(&app, "POST", "/mcp/fakey/explode", None).await;
    assert_eq!(s, 500);

    // DELETE /mcp/{name}/auth: known → {success:true}, unknown → 404 tag
    let (s, b) = call(&app, "DELETE", "/mcp/fakey/auth", None).await;
    assert_eq!(s, 200);
    assert_eq!(std::str::from_utf8(&b).unwrap(), r#"{"success":true}"#);
    let (s, b) = call(&app, "DELETE", "/mcp/nope/auth", None).await;
    assert_eq!(s, 404);
    assert!(String::from_utf8_lossy(&b).contains("McpServerNotFoundError"));
}

#[tokio::test]
async fn hub_unavailable_is_503() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path(), Payloads::default()); // mcp OnceLock unset
    let app = ocserve_http::router(st);
    let (s, b) = call(&app, "POST", "/mcp/x/connect", None).await;
    assert_eq!(s, 503);
    assert!(String::from_utf8_lossy(&b).contains("McpUnavailable"));
}

#[tokio::test]
async fn auth_overlay_crud_and_provider_methods() {
    let dir = tempfile::tempdir().unwrap();
    // LIVE shape: providers is a LIST of {id, ...} (caught by the W5 battery
    // when the dict assumption returned {})
    let payloads = Payloads {
        config_providers: serde_json::json!({
            "providers": [{"id": "deepseek"}, {"id": "openrouter"}],
            "default": {}
        }),
        ..Default::default()
    };
    let cp_seed = payloads.config_providers.clone();
    let st = state(dir.path(), payloads);
    // reloader counter (boot-injected analog) — preserves the seeded
    // config_providers like Runtime::load_for would
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let h2 = hits.clone();
    let cp = cp_seed;
    *st.reloader.write() = Some(std::sync::Arc::new(move || {
        h2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok((
            Payloads {
                config_providers: cp.clone(),
                ..Default::default()
            },
            ocserve_http::LlmRegistry::default(),
        ))
    }));
    let app = ocserve_http::router(st.clone());

    // PUT /auth/{pid} stores verbatim + reloads + true
    let (s, b) = call(
        &app,
        "PUT",
        "/auth/deepseek",
        Some(serde_json::json!({"type": "api", "key": "sk-test"})),
    )
    .await;
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
    assert_eq!(std::str::from_utf8(&b).unwrap(), "true");
    let overlay_path = ocserve_store::writer::db_path(dir.path())
        .parent()
        .unwrap()
        .join("auth-overlay.json");
    let overlay: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&overlay_path).unwrap()).unwrap();
    assert_eq!(overlay["deepseek"]["key"], "sk-test");
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "reloader ran"
    );

    // non-object payload →400
    let (s, _) = call(&app, "PUT", "/auth/bad", Some(serde_json::json!("oops"))).await;
    assert_eq!(s, 400);

    // DELETE removes it + reloads again (idempotent even if missing)
    let (s, _) = call(&app, "DELETE", "/auth/deepseek", None).await;
    assert_eq!(s, 200);
    let overlay: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&overlay_path).unwrap()).unwrap();
    assert!(overlay.get("deepseek").is_none());
    assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 2);
    let (s, _) = call(&app, "DELETE", "/auth/never-existed", None).await;
    assert_eq!(s, 200, "idempotent");

    // GET /provider/auth — configured providers get the api method
    let (s, b) = call(&app, "GET", "/provider/auth", None).await;
    assert_eq!(s, 200);
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["deepseek"][0]["type"], "api");
    assert_eq!(v["deepseek"][0]["label"], "API Key");
    assert!(v.get("openrouter").is_some());
}

#[tokio::test]
async fn global_dispose_emits_event_and_returns_true() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path(), Payloads::default());
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let h2 = hits.clone();
    *st.reloader.write() = Some(std::sync::Arc::new(move || {
        h2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok((Payloads::default(), ocserve_http::LlmRegistry::default()))
    }));
    let mut rx = st.bus.subscribe();
    let app = ocserve_http::router(st.clone());
    let (s, b) = call(&app, "POST", "/global/dispose", None).await;
    assert_eq!(s, 200);
    assert_eq!(std::str::from_utf8(&b).unwrap(), "true");
    // durable row
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE type='server.instance.disposed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "durable disposed event");
    // live frame
    let mut saw = false;
    while let Ok(f) = rx.try_recv() {
        if f.to_string().contains("server.instance.disposed") {
            saw = true;
        }
    }
    assert!(saw, "SSE frame published");
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "reloader ran"
    );
}

/// P0f: /experimental/tool(/ids) — v1 shapes (ToolList + ToolIDs), query
/// validation mirroring Effect's BadRequest, ids ⊇ builtins.
#[tokio::test]
async fn experimental_tool_routes() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path(), Payloads::default());
    let app = ocserve_http::router(st);

    // ids: plain string array containing every builtin
    let (status, bytes) = call(&app, "GET", "/experimental/tool/ids", None).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<String> = serde_json::from_slice(&bytes).unwrap();
    for want in ["bash", "read", "write", "edit", "glob", "grep"] {
        assert!(
            ids.contains(&want.to_string()),
            "ids missing {want}: {ids:?}"
        );
    }

    // full list: [{id, description, parameters}] keyed to the same catalog
    let (status, bytes) = call(
        &app,
        "GET",
        "/experimental/tool?provider=fake&model=m",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(items.len(), ids.len(), "list and ids must align");
    let bash = items
        .iter()
        .find(|i| i["id"] == "bash")
        .expect("bash in list");
    assert_eq!(
        bash["description"],
        "Run a shell command and return its output."
    );
    assert_eq!(bash["parameters"]["type"], "object");

    // missing query params → 400 BadRequest (Effect query validation parity)
    let (status, bytes) = call(&app, "GET", "/experimental/tool", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(err["name"], "BadRequest");
}
