//! M4a gates (TESTING §4): framing, handshake, pagination, statuses,
//! namespacing — hermetic fake stdio server (NDJSON), no network.

use refine_mcp::{Kind, McpClient, McpHub, ServerCfg, parse_config, sanitize, tool_name};
use serde_json::json;

const FAKE_SERVER: &str = r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    msg = json.loads(line)
    if "id" not in msg: continue  # notification
    method = msg.get("method")
    mid = msg["id"]
    if method == "initialize":
        resp = {"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":"2025-06-18","capabilities":{"tools":{}},
            "serverInfo":{"name":"fake","version":"1"}}}
    elif method == "tools/list":
        # cursor pagination: first page + cursor, second page no cursor
        params = msg.get("params") or {}
        if params.get("cursor") == "p2":
            resp = {"jsonrpc":"2.0","id":mid,"result":{"tools":[
                {"name":"tool-b","description":"B","inputSchema":{"type":"object","properties":{"x":{"type":"string"}}}}]}}
        else:
            resp = {"jsonrpc":"2.0","id":mid,"result":{"tools":[
                {"name":"tool-a","description":"A","inputSchema":{"type":"object","properties":{}}}],
                "nextCursor":"p2"}}
    elif method == "tools/call":
        name = msg["params"]["name"]; args = msg["params"].get("arguments") or {}
        if name == "boom":
            resp = {"jsonrpc":"2.0","id":mid,"result":{"isError":True,
                "content":[{"type":"text","text":"tool exploded"}]}}
        else:
            resp = {"jsonrpc":"2.0","id":mid,"result":{"content":[
                {"type":"text","text":"echo: "+name+" "+json.dumps(args)}]}}
    else:
        resp = {"jsonrpc":"2.0","id":mid,"result":{}}
    sys.stdout.write(json.dumps(resp)+"\n"); sys.stdout.flush()
"#;

fn fake_cfg(name: &str) -> ServerCfg {
    let dir = std::env::temp_dir().join(format!("refine-mcp-test-{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("fake_mcp.py");
    std::fs::write(&script, FAKE_SERVER).unwrap();
    ServerCfg {
        name: format!("fake-{name}"),
        enabled: true,
        kind: Kind::Local {
            command: vec!["python3".into(), script.to_string_lossy().into()],
            env: vec![],
        },
    }
}

#[test]
fn sanitize_and_tool_name_match_upstream_catalog() {
    assert_eq!(sanitize("context7"), "context7");
    assert_eq!(sanitize("browser-harness"), "browser-harness");
    assert_eq!(sanitize("my server:v1"), "my_server_v1");
    // catalog.toolName = sanitize(server) + "_" + sanitize(tool)
    assert_eq!(
        tool_name("context7", "resolve-library-id"),
        "context7_resolve-library-id"
    );
    assert_eq!(tool_name("my server", "do/it"), "my_server_do_it");
}

#[test]
fn parse_config_local_remote_and_disabled() {
    let v = json!({
        "a": {"type": "local", "command": ["prog", "--flag"], "environment": {"K": "V"}},
        "b": {"type": "remote", "url": "https://x/mcp", "headers": {"H": "1"}, "enabled": true},
        "c": {"type": "local", "command": ["dead"], "enabled": false}
    });
    let cfgs = parse_config(&v);
    assert_eq!(cfgs.len(), 3);
    match &cfgs[0].kind {
        Kind::Local { command, env } => {
            assert_eq!(command, &vec!["prog".to_string(), "--flag".to_string()]);
            assert_eq!(env[0], ("K".into(), "V".into()));
        }
        _ => panic!("a is local"),
    }
    match &cfgs[1].kind {
        Kind::Remote { url, headers } => {
            assert_eq!(url, "https://x/mcp");
            assert_eq!(headers[0], ("H".into(), "1".into()));
        }
        _ => panic!("b is remote"),
    }
    assert!(!cfgs[2].enabled, "enabled=false preserved");
}

#[tokio::test]
async fn stdio_handshake_list_paginated_and_call() {
    let cfg = fake_cfg("basic");
    let mut client = McpClient::connect(&cfg).await.expect("spawn");
    let init = client.initialize().await.expect("initialize");
    assert_eq!(init["serverInfo"]["name"], "fake");
    let tools = client.list_tools().await.expect("list");
    assert_eq!(tools.len(), 2, "two pages merged");
    assert_eq!(tools[0]["name"], "tool-a");
    assert_eq!(tools[1]["name"], "tool-b");
    let out = client
        .call_tool("tool-a", json!({"q": "hi"}))
        .await
        .expect("call");
    assert!(out.contains("echo: tool-a"), "got: {out}");
    let err = client.call_tool("boom", json!({})).await;
    assert!(err.is_err(), "isError → Err (negative control)");
    assert!(
        err.unwrap_err().to_string().contains("tool exploded"),
        "error text propagated"
    );
    client.close().await;
}

#[tokio::test]
async fn hub_statuses_and_tool_namespacing() {
    let good = fake_cfg("good");
    let bad = ServerCfg {
        name: "missing-bin".into(),
        enabled: true,
        kind: Kind::Local {
            command: vec!["definitely-not-a-real-binary-xyz".into()],
            env: vec![],
        },
    };
    let hub = McpHub::probe_all(&[good.clone(), bad]).await;
    let st = hub.statuses();
    assert_eq!(st["fake-good"]["status"], "connected");
    assert_eq!(st["missing-bin"]["status"], "failed");
    assert!(
        st["missing-bin"]["error"].is_string(),
        "failure carries an error string (freeze shape §1146)"
    );
    // tool schemas are namespaced sanitize(server)_sanitize(tool)
    let schemas = hub.tool_schemas().await;
    assert_eq!(schemas.len(), 2);
    let names: Vec<&str> = schemas
        .iter()
        .filter_map(|s| s["function"]["name"].as_str())
        .collect();
    assert!(names.contains(&"fake-good_tool-a"), "got {names:?}");
    // dispatch by namespaced name
    let called = hub
        .call("fake-good_tool-a", json!({"k": 1}))
        .await
        .expect("dispatched");
    assert!(called.expect("ok").contains("tool-a"));
    // non-MCP name → None (caller falls through to builtins)
    assert!(hub.call("bash", json!({})).await.is_none());
}
