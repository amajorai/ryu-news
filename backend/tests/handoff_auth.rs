//! The loopback MCP handoff route runs the sealed store's tools for a keyless
//! process, so only the per-run handoff token may open it. Any other loopback
//! caller (no header, Core's ext token, a guessed token) must get 401 without
//! the tool ever running.

use ryu_news::api::{handoff_routes, HANDOFF_MCP_ROUTE};
use ryu_news::models::DEFAULT_WORKSPACE_ID;
use ryu_news::state::{AppState, Config};
use ryu_news::store::NewsStore;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const CORE_EXT_TOKEN: &str = "core-ext-token-for-the-mounted-api";
const SEALED_TOPIC: &str = "Sealed acquisition watch";

/// POST the handoff route over a real loopback socket; returns (status, body).
async fn post(port: u16, authorization: Option<&str>, body: &Value) -> (u16, String) {
    let payload = body.to_string();
    let mut head = format!(
        "POST {HANDOFF_MCP_ROUTE} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        payload.len()
    );
    if let Some(value) = authorization {
        head.push_str(&format!("Authorization: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connects");
    stream.write_all(head.as_bytes()).await.expect("writes");
    stream.write_all(payload.as_bytes()).await.expect("writes");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("reads");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let body = text
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_owned();
    (status, body)
}

#[tokio::test]
async fn only_the_handoff_token_opens_the_mcp_handoff_route() {
    let store = NewsStore::open_in_memory().expect("opens");
    store
        .ensure_workspace(DEFAULT_WORKSPACE_ID, "Default")
        .await
        .expect("workspace");
    store
        .create_topic(
            DEFAULT_WORKSPACE_ID,
            SEALED_TOPIC,
            "merger",
            &json!({ "kind": "term", "value": "merger" }),
        )
        .await
        .expect("creates")
        .expect("a new topic");
    let mut config = Config::from_env(0);
    config.token = Some(CORE_EXT_TOKEN.to_owned());
    config.poller_enabled = false;
    let state = AppState::new(store, config);

    let handoff_token = "per-run-handoff-token-0123456789abcdef".to_owned();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let port = listener.local_addr().expect("addr").port();
    let app = handoff_routes(state, handoff_token.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let call = json!({ "name": "topics", "arguments": {} });
    let core_ext = format!("Bearer {CORE_EXT_TOKEN}");
    let wrong = format!("Bearer {handoff_token}x");
    let raw_token = handoff_token.clone();
    for (label, authorization) in [
        ("no Authorization", None),
        ("the Core ext token", Some(core_ext.as_str())),
        ("a wrong bearer token", Some(wrong.as_str())),
        (
            "the handoff token without the Bearer scheme",
            Some(raw_token.as_str()),
        ),
        ("an empty bearer token", Some("Bearer ")),
    ] {
        let (status, body) = post(port, authorization, &call).await;
        assert_eq!(
            status, 401,
            "{label} must be rejected, got {status}: {body}"
        );
        assert!(
            !body.contains(SEALED_TOPIC) && !body.contains("isError"),
            "{label} reached the tool: {body}"
        );
    }

    let (status, body) = post(port, Some(&format!("Bearer {handoff_token}")), &call).await;
    assert_eq!(status, 200, "{body}");
    let result: Value = serde_json::from_str(&body).expect("a JSON tool result");
    assert_eq!(result["isError"], json!(false), "{result}");
    let text = result["content"][0]["text"].as_str().expect("text content");
    assert!(
        text.contains(SEALED_TOPIC),
        "the tool ran on the store: {text}"
    );

    server.abort();
}
