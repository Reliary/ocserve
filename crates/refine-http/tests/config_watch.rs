//! K-CONFIG-RELOAD: config hot-reload (H1 watcher + H2 registry swap) —
//! external edits picked up by reconcile, fail-safe on corrupt config,
//! MCP section diff → connect/disconnect, poll loop e2e + disabled control.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use refine_http::watch::{WatchOpts, observe, watch_loop};
use refine_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

fn state(dir: &std::path::Path, payloads: Payloads) -> Arc<AppState> {
    let db = refine_store::writer::db_path(dir);
    let writer = Arc::new(refine_store::Writer::spawn(db.clone()).unwrap());
    let blobs = Arc::new(refine_store::BlobStore::new(dir.join("blobs")).unwrap());
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

async fn get_config(app: &axum::Router) -> Value {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Fake reloader reading `path` (modeled on Runtime::load_for's failure
/// contract: bad JSON → Err, nothing swaps). Registry marker = "swapped".
fn file_reloader(path: std::path::PathBuf) -> refine_http::ConfigReloader {
    Arc::new(move || {
        let p = Payloads {
            config: serde_json::from_str::<Value>(&std::fs::read_to_string(&path)?)?,
            ..Default::default()
        };
        Ok((
            p,
            LlmRegistry {
                default_model: ("swapped".into(), "marker".into()),
                ..Default::default()
            },
        ))
    })
}

#[tokio::test]
async fn external_edit_is_detected_and_reconciled() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.json");
    std::fs::write(&cfg, r#"{"theme":"dark"}"#).unwrap();
    let st = state(dir.path(), Payloads::default());
    *st.reloader.write() = Some(file_reloader(cfg.clone()));
    st.watch.write().paths = vec![cfg.clone()];

    let app = refine_http::router(st.clone());
    observe(&st);
    // boot parity: recorded tuple → no spurious change (stat-gate control)
    assert!(!refine_http::watch::changed(&st), "no change → no reload");

    std::fs::write(&cfg, r#"{"theme":"light"}"#).unwrap();
    assert!(
        refine_http::watch::changed(&st),
        "external edit detected (mtime/len tuple)"
    );

    refine_http::watch::reconcile(&st).await.expect("reconcile");
    let v = get_config(&app).await;
    assert_eq!(v["theme"], "light", "GET /config serves the external edit");
    assert_eq!(
        st.llm.read().default_model,
        ("swapped".into(), "marker".into()),
        "H2: registry swapped in the same reconcile"
    );
    assert!(
        !refine_http::watch::changed(&st),
        "observed advanced after success"
    );
    assert!(
        st.reconcile_lock.try_lock().is_ok(),
        "lock released after reconcile (no leak)"
    );
}

#[tokio::test]
async fn corrupt_edit_keeps_old_state_then_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.json");
    std::fs::write(&cfg, r#"{"theme":"dark"}"#).unwrap();
    let st = state(
        dir.path(),
        Payloads {
            config: json!({"theme": "dark"}),
            ..Default::default()
        },
    );
    *st.reloader.write() = Some(file_reloader(cfg.clone()));
    st.watch.write().paths = vec![cfg.clone()];
    let app = refine_http::router(st.clone());
    observe(&st);

    // half-saved file (editor mid-write): parse fails → NOTHING swaps
    std::fs::write(&cfg, r#"{"theme":"ligt"#).unwrap();
    let err = refine_http::watch::reconcile(&st).await;
    assert!(err.is_err(), "corrupt config must fail the reload");
    let v = get_config(&app).await;
    assert_eq!(
        v["theme"], "dark",
        "FAIL-SAFE: old config keeps serving after a broken reload"
    );
    assert_eq!(
        st.llm.read().default_model,
        ("fake".into(), "m".into()),
        "registry untouched on failure"
    );
    // observed NOT advanced → watcher will retry (retry-semantics control)
    assert!(
        refine_http::watch::changed(&st),
        "failed load keeps the change pending for retry"
    );

    // write completes → next reconcile heals
    std::fs::write(&cfg, r#"{"theme":"light"}"#).unwrap();
    refine_http::watch::reconcile(&st).await.expect("heal");
    let v = get_config(&app).await;
    assert_eq!(v["theme"], "light", "recovers once the file is valid");
}

/// Minimal hermetic MCP server: initialize + tools/list (one tool named
/// `probe`); name/tool differ per script so cfg equality is exact.
fn fake_mcp_script(dir: &std::path::Path, tag: &str) -> std::path::PathBuf {
    let script = dir.join(format!("fake_{tag}.py"));
    std::fs::write(
        &script,
        format!(
            r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    msg = json.loads(line)
    if "id" not in msg: continue
    method = msg.get("method"); mid = msg["id"]
    if method == "initialize":
        resp = {{"jsonrpc":"2.0","id":mid,"result":{{
            "protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},
            "serverInfo":{{"name":"{tag}","version":"1"}}}}}}
    elif method == "tools/list":
        resp = {{"jsonrpc":"2.0","id":mid,"result":{{"tools":[
            {{"name":"probe","description":"{tag} probe",
             "inputSchema":{{"type":"object","properties":{{}}}}}}]}}}}
    elif method == "tools/call":
        resp = {{"jsonrpc":"2.0","id":mid,"result":{{"content":[
            {{"type":"text","text":"{tag}-ok"}}]}}}}
    else:
        resp = {{"jsonrpc":"2.0","id":mid,"result":{{}}}}
    sys.stdout.write(json.dumps(resp)+"\n"); sys.stdout.flush()
"#
        ),
    )
    .unwrap();
    script
}

fn mcp_cfg(_name: &str, script: &std::path::Path) -> Value {
    json!({"type": "local", "command": ["python3", script.to_string_lossy()]})
}

#[tokio::test]
async fn mcp_section_diff_connects_and_disconnects() {
    let dir = tempfile::tempdir().unwrap();
    let script_a = fake_mcp_script(dir.path(), "aaa");
    let script_b = fake_mcp_script(dir.path(), "bbb");

    let cfg_a = refine_mcp::ServerCfg {
        name: "srv-a".into(),
        enabled: true,
        kind: refine_mcp::Kind::Local {
            command: vec!["python3".into(), script_a.to_string_lossy().into()],
            env: vec![],
        },
    };
    let hub =
        std::sync::Arc::new(refine_mcp::McpHub::probe_all(std::slice::from_ref(&cfg_a)).await);
    assert_eq!(
        hub.statuses()["srv-a"]["status"],
        "connected",
        "server A up at boot"
    );

    // served config = {a}; reloader's new config = {a, b}
    let old_cfg = json!({"mcp": {"srv-a": mcp_cfg("srv-a", &script_a)}});
    let new_cfg = json!({"mcp": {
        "srv-a": mcp_cfg("srv-a", &script_a),
        "srv-b": mcp_cfg("srv-b", &script_b)
    }});
    let p = Payloads {
        config: old_cfg.clone(),
        ..Default::default()
    };
    let new_cfg2 = new_cfg.clone();
    let st = state(dir.path(), p);
    assert!(st.mcp.set(hub.clone()).is_ok(), "hub set once");
    *st.reloader.write() = Some(Arc::new(move || {
        let p = Payloads {
            config: new_cfg2.clone(),
            ..Default::default()
        };
        Ok((p, LlmRegistry::default()))
    }));

    // reconcile: {a} → {a,b} → only b connects
    refine_http::watch::reconcile(&st)
        .await
        .expect("add reconcile");
    let st_json = hub.statuses();
    assert_eq!(st_json["srv-a"]["status"], "connected", "A untouched");
    assert_eq!(
        st_json["srv-b"]["status"], "connected",
        "B added + connected"
    );
    let tools = hub.tool_schemas().await;
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with("srv-b_")),
        "rescan picked up B's tools: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.starts_with("srv-a_")),
        "A's tools still served: {names:?}"
    );

    // reconcile back to {a} → b disconnects, cfg dropped
    let back = json!({"mcp": {"srv-a": mcp_cfg("srv-a", &script_a)}});
    let st2 = state(dir.path(), Payloads::default());
    assert!(st2.mcp.set(hub.clone()).is_ok(), "hub set once"); // share the same hub
    let back2 = back.clone();
    *st2.reloader.write() = Some(Arc::new(move || {
        let p = Payloads {
            config: back2.clone(),
            ..Default::default()
        };
        Ok((p, LlmRegistry::default()))
    }));
    // seed st2 payloads as the OLD served value so the diff sees removal
    *st2.payloads.write() = Payloads {
        config: new_cfg.clone(),
        ..Default::default()
    };
    refine_http::watch::reconcile(&st2)
        .await
        .expect("remove reconcile");
    let st_json = hub.statuses();
    assert_eq!(
        st_json["srv-b"]["status"], "disconnected",
        "B removed + disconnected"
    );
    assert_eq!(
        st_json["srv-a"]["status"], "connected",
        "A survives removal"
    );
    assert!(
        hub.connect("srv-b").await.is_err(),
        "dropped cfg cannot resurrect a removed server (drop_cfg)"
    );
}

#[tokio::test]
async fn watch_loop_polls_edits_and_disabled_is_inert() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.json");
    std::fs::write(&cfg, r#"{"v":1}"#).unwrap();
    let st = state(
        dir.path(),
        Payloads {
            config: json!({"v": 1}),
            ..Default::default()
        },
    );
    *st.reloader.write() = Some(file_reloader(cfg.clone()));
    st.watch.write().paths = vec![cfg.clone()];
    let app = refine_http::router(st.clone());
    observe(&st);

    // disabled loop (kill-switch negative control): edit → no pickup
    let off = tokio::spawn(watch_loop(
        st.clone(),
        WatchOpts {
            enabled: false,
            interval_ms: 50,
        },
    ));
    std::fs::write(&cfg, r#"{"v":99}"#).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        get_config(&app).await["v"],
        1,
        "REFINE_CONFIG_WATCH=0 equivalent: loop must stay inert"
    );
    off.await.unwrap();

    // active loop: another edit → picked up within a few ticks
    let on = tokio::spawn(watch_loop(
        st.clone(),
        WatchOpts {
            enabled: true,
            interval_ms: 50,
        },
    ));
    std::fs::write(&cfg, r#"{"v":2}"#).unwrap();
    let mut got = 0;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        got = get_config(&app).await["v"].as_i64().unwrap_or(0);
        if got == 2 {
            break;
        }
    }
    assert_eq!(got, 2, "watch loop applies the external edit");
    on.abort();
}
