use super::*;
use crate::seal_keys::KeySource;
use base64::Engine as _;
use ryu_crypto::PluginCipher;
use std::path::Path;

const FEED: &str = "https://private.test/feed.xml?token=quokka-feed-secret";
const WATCH: &str = "quokka watch name";

fn keys(n: u8) -> PluginKeys {
    PluginKeys::sealing(
        PluginCipher::from_subkey_b64(
            PLUGIN_ID,
            &base64::engine::general_purpose::STANDARD.encode([n; 32]),
        )
        .unwrap(),
        KeySource::Delivered,
    )
}

fn db() -> PathBuf {
    std::env::temp_dir()
        .join(format!("ryu-news-at-rest-{}", uuid::Uuid::new_v4()))
        .join(STORE)
}

fn on_disk(path: &Path, needle: &str) -> bool {
    let mut bytes = std::fs::read(path).unwrap_or_default();
    bytes.extend(std::fs::read(format!("{}-wal", path.display())).unwrap_or_default());
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn source(title: &str, feed: &str) -> NewSource {
    NewSource {
        title: title.into(),
        feed_url: feed.into(),
        site_url: None,
        kind: SourceKind::Rss,
    }
}

async fn seed(store: &NewsStore) {
    for (title, feed) in [("Zeta", FEED), ("Alpha", "https://b.test/feed.xml")] {
        assert!(store
            .create_source(DEFAULT_WORKSPACE_ID, &source(title, feed))
            .await
            .unwrap()
            .is_some());
    }
    assert!(store
        .create_topic(
            DEFAULT_WORKSPACE_ID,
            WATCH,
            "quokka",
            &serde_json::json!({"t": "quokka"})
        )
        .await
        .unwrap()
        .is_some());
}

fn raw(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT feed_url || '|' || feed_url_bi FROM sources
             UNION ALL SELECT name || '|' || name_bi FROM topics",
        )
        .unwrap();
    let mut rows: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn sources_and_watches_are_sealed_and_still_unique() {
    let path = db();
    let store = NewsStore::open_with_keys(path.clone(), &keys(1)).unwrap();
    seed(&store).await;
    let sources = store.list_sources(DEFAULT_WORKSPACE_ID).await.unwrap();
    let titles: Vec<&str> = sources.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, ["Alpha", "Zeta"], "sorted after opening");
    assert_eq!(sources[1].feed_url, FEED);
    // Subscribing twice and a second watch of one name are still no-ops.
    assert!(store
        .create_source(DEFAULT_WORKSPACE_ID, &source("Again", FEED))
        .await
        .unwrap()
        .is_none());
    assert!(store
        .create_topic(
            DEFAULT_WORKSPACE_ID,
            WATCH,
            "other",
            &serde_json::json!(null)
        )
        .await
        .unwrap()
        .is_none());
    drop(store);
    assert!(!on_disk(&path, "quokka-feed-secret"));
    assert!(!on_disk(&path, WATCH));
    let conn = Connection::open(&path).unwrap();
    let cipher = keys(1).content_cipher(STORE).unwrap();
    ryu_crypto::content_sqlite::assert_all_open(&conn, &cipher, &column_plans().unwrap()).unwrap();
}

#[tokio::test]
async fn duplicates_from_an_older_build_fold_instead_of_failing_the_open() {
    let path = db();
    let store = NewsStore::open_with_keys(path.clone(), &keys(1)).unwrap();
    seed(&store).await;
    drop(store);
    // A pre-sealing build after a downgrade: no plaintext unique index any
    // more, so re-adding the feed and the watch inserts duplicates in the
    // clear with no token.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO sources (id, workspace_id, title, feed_url, kind, enabled,
                                  consecutive_failures, created_at)
             VALUES ('src_dup', ?1, 'Zeta again', ?2, 'rss', 1, 0, 9999999999999)",
            params![DEFAULT_WORKSPACE_ID, FEED],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO topics (id, workspace_id, name, query, ast, enabled,
                                 created_at, updated_at)
             VALUES ('top_dup', ?1, ?2, 'quokka', 'null', 1, 9999999999999,
                     9999999999999)",
            params![DEFAULT_WORKSPACE_ID, WATCH],
        )
        .unwrap();
    }
    let store = NewsStore::open_with_keys(path.clone(), &keys(1))
        .expect("a duplicate must not fail the open");
    let sources = store.list_sources(DEFAULT_WORKSPACE_ID).await.unwrap();
    let feeds: Vec<&str> = sources
        .iter()
        .filter(|s| s.feed_url == FEED)
        .map(|s| s.title.as_str())
        .collect();
    assert_eq!(feeds, ["Zeta"], "the first subscription survives");
    let topics = store.list_topics(DEFAULT_WORKSPACE_ID).await.unwrap();
    assert_eq!(topics.iter().filter(|t| t.name == WATCH).count(), 1);
    assert!(topics.iter().all(|t| t.id != "top_dup"));
    drop(store);
    // And the next start is clean too.
    let store = NewsStore::open_with_keys(path.clone(), &keys(1)).unwrap();
    assert_eq!(
        store
            .list_sources(DEFAULT_WORKSPACE_ID)
            .await
            .unwrap()
            .len(),
        2
    );
    drop(store);
    assert!(!on_disk(&path, "quokka-feed-secret"));
}

#[tokio::test]
async fn legacy_rows_are_sealed_tokenized_once_and_still_deduplicate() {
    let path = db();
    seed(&NewsStore::open_with_keys(path.clone(), &PluginKeys::compat()).unwrap()).await;
    assert!(on_disk(&path, "quokka-feed-secret"));
    let store = NewsStore::open_with_keys(path.clone(), &keys(2)).unwrap();
    assert!(store
        .create_source(DEFAULT_WORKSPACE_ID, &source("Again", FEED))
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store.list_topics(DEFAULT_WORKSPACE_ID).await.unwrap()[0].name,
        WATCH
    );
    drop(store);
    assert!(!on_disk(&path, "quokka-feed-secret"));
    assert!(!on_disk(&path, WATCH));
    let first = raw(&path);
    assert!(first
        .iter()
        .all(|row| row.starts_with("encb:v1:") && row.contains("|bi1:")));
    drop(NewsStore::open_with_keys(path.clone(), &keys(2)).unwrap());
    assert_eq!(raw(&path), first);
}

#[tokio::test]
async fn a_copied_value_is_skipped_and_other_keys_refuse_writes() {
    let path = db();
    let store = NewsStore::open_with_keys(path.clone(), &keys(3)).unwrap();
    seed(&store).await;
    store
        .conn
        .lock()
        .await
        .execute(
            "UPDATE sources SET title = (SELECT title FROM sources ORDER BY id LIMIT 1)",
            [],
        )
        .unwrap();
    assert_eq!(
        store
            .list_sources(DEFAULT_WORKSPACE_ID)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.seal_health()["unreadable_rows"], 1);
    drop(store);
    for other in [keys(4), PluginKeys::compat()] {
        let wrong = NewsStore::open_with_keys(path.clone(), &other).unwrap();
        assert_eq!(wrong.seal_health()["state"], "unhealthy");
        assert!(wrong.locked_out().await);
        assert!(wrong
            .create_source(DEFAULT_WORKSPACE_ID, &source("New", "https://c.test/f"))
            .await
            .is_err());
    }
}
