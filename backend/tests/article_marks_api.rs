//! The reader marks and story follow, through the real `routes(state)` router.
//!
//! A mark POST with no body means "on"; `{on:false}` clears it; archive takes the
//! article out of `GET /articles`; an unknown id is a 404 rather than a silent 200.
//! The same rules hold for following a story.

use ryu_news::api::routes;
use ryu_news::models::{NewArticle, NewSource, SourceKind, DEFAULT_WORKSPACE_ID};
use ryu_news::state::{AppState, Config};
use ryu_news::store::NewsStore;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// One HTTP/1.1 request over a real loopback socket; returns (status, JSON body).
async fn send(port: u16, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let payload = body.map(Value::to_string).unwrap_or_default();
    let mut head =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if body.is_some() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    } else if method == "POST" {
        head.push_str("Content-Length: 0\r\n");
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
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

struct Fixture {
    port: u16,
    state: AppState,
    article_id: String,
    story_id: String,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

async fn fixture() -> Fixture {
    let store = NewsStore::open_in_memory().expect("store");
    let source = store
        .create_source(
            DEFAULT_WORKSPACE_ID,
            &NewSource {
                title: "Wire".into(),
                feed_url: "https://wire.test/feed".into(),
                site_url: None,
                kind: SourceKind::Rss,
            },
        )
        .await
        .expect("create")
        .expect("fresh");
    let now = ryu_news::models::now_ms();
    let article = store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &NewArticle {
                source_id: source.id,
                guid: None,
                url: "https://wire.test/a".into(),
                canonical_url: "https://wire.test/a".into(),
                title: "Regulator opens inquiry".into(),
                author: None,
                summary: None,
                content: None,
                published_at: now,
                simhash: 7,
            },
            now,
        )
        .await
        .expect("insert")
        .expect("new");
    let story = store
        .create_story(DEFAULT_WORKSPACE_ID, &article.id, &[], &[], now)
        .await
        .expect("story");
    let mut config = Config::from_env(0);
    config.poller_enabled = false;
    let state = AppState::new(store, config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let port = listener.local_addr().expect("addr").port();
    let app = routes(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    Fixture {
        port,
        state,
        article_id: article.id,
        story_id: story.id,
        server,
    }
}

async fn article_ids(port: u16) -> Vec<String> {
    let (status, body) = send(port, "GET", "/articles", None).await;
    assert_eq!(status, 200, "{body}");
    body["articles"]
        .as_array()
        .expect("articles")
        .iter()
        .map(|a| a["id"].as_str().expect("id").to_owned())
        .collect()
}

#[tokio::test]
async fn read_and_save_default_to_on_and_clear_with_on_false() {
    let f = fixture().await;
    for (verb, field) in [("read", "read_at"), ("save", "saved_at")] {
        let path = format!("/articles/{}/{verb}", f.article_id);
        let (status, body) = send(f.port, "POST", &path, None).await;
        assert_eq!((status, &body), (200, &json!({ "ok": true })), "{verb}");
        let stored = f
            .state
            .store
            .get_article(&f.article_id)
            .await
            .unwrap()
            .unwrap();
        let value = serde_json::to_value(&stored).unwrap();
        assert!(value[field].is_i64(), "no body must SET {field}: {value}");

        let (status, _) = send(f.port, "POST", &path, Some(&json!({ "on": false }))).await;
        assert_eq!(status, 200);
        let stored = f
            .state
            .store
            .get_article(&f.article_id)
            .await
            .unwrap()
            .unwrap();
        let value = serde_json::to_value(&stored).unwrap();
        assert!(value[field].is_null(), "{{on:false}} must clear {field}");
    }
    f.server.abort();
}

#[tokio::test]
async fn archive_removes_the_article_from_the_feed_and_unarchive_restores_it() {
    let f = fixture().await;
    assert_eq!(article_ids(f.port).await, vec![f.article_id.clone()]);
    let path = format!("/articles/{}/archive", f.article_id);
    assert_eq!(send(f.port, "POST", &path, None).await.0, 200);
    assert!(article_ids(f.port).await.is_empty());
    let (status, body) = send(f.port, "GET", "/articles?archived=true", None).await;
    assert_eq!(status, 200);
    assert_eq!(body["articles"][0]["id"], json!(f.article_id));
    let off = json!({ "on": false });
    assert_eq!(send(f.port, "POST", &path, Some(&off)).await.0, 200);
    assert_eq!(article_ids(f.port).await, vec![f.article_id.clone()]);
    f.server.abort();
}

#[tokio::test]
async fn follow_defaults_to_true_and_followed_false_unfollows() {
    let f = fixture().await;
    let path = format!("/stories/{}/follow", f.story_id);
    let followed = |state: AppState, id: String| async move {
        state.store.get_story(&id).await.unwrap().unwrap().followed
    };
    assert!(!followed(f.state.clone(), f.story_id.clone()).await);
    let (status, body) = send(f.port, "POST", &path, None).await;
    assert_eq!((status, body), (200, json!({ "ok": true })));
    assert!(followed(f.state.clone(), f.story_id.clone()).await);
    let off = json!({ "followed": false });
    assert_eq!(send(f.port, "POST", &path, Some(&off)).await.0, 200);
    assert!(!followed(f.state.clone(), f.story_id.clone()).await);
    f.server.abort();
}

#[tokio::test]
async fn an_unknown_article_or_story_id_is_a_404() {
    let f = fixture().await;
    for path in [
        "/articles/art_missing/read",
        "/articles/art_missing/save",
        "/articles/art_missing/archive",
        "/stories/sty_missing/follow",
    ] {
        let (status, body) = send(f.port, "POST", path, None).await;
        assert_eq!(status, 404, "{path}: {body}");
    }
    // The real article was untouched by the misses.
    let stored = f
        .state
        .store
        .get_article(&f.article_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.is_read() && !stored.is_saved() && !stored.is_archived());
    f.server.abort();
}
