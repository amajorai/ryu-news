//! The topic-watch matching pass: which (topic, article) pairs it records.
//!
//! Only an ENABLED topic over an article published in the last 24h may match; a
//! second pass over the same data counts nothing because the match row is
//! idempotent; and one topic whose stored AST no longer deserializes must not stop
//! every other watch from matching.

use ryu_news::models::{NewArticle, NewSource, SourceKind, DEFAULT_WORKSPACE_ID};
use ryu_news::query;
use ryu_news::service::match_topics;
use ryu_news::state::{AppState, Config};
use ryu_news::store::NewsStore;
use serde_json::json;

const HOUR_MS: i64 = 3_600_000;
const NOW: i64 = 1_786_320_000_000;

fn article(source_id: &str, slug: &str, title: &str, published_at: i64) -> NewArticle {
    let url = format!("https://wire.test/{slug}");
    NewArticle {
        source_id: source_id.to_owned(),
        guid: None,
        url: url.clone(),
        canonical_url: url,
        title: title.to_owned(),
        author: None,
        summary: None,
        content: None,
        published_at,
        simhash: slug.len() as u64,
    }
}

async fn topic(state: &AppState, name: &str, raw: &str) -> String {
    let ast = serde_json::to_value(query::parse(raw).expect("parses")).expect("serializes");
    state
        .store
        .create_topic(DEFAULT_WORKSPACE_ID, name, raw, &ast)
        .await
        .expect("create")
        .expect("unique name")
        .id
}

#[tokio::test]
async fn only_enabled_topics_match_recent_articles_and_a_rerun_counts_nothing() {
    let state = AppState::new(
        NewsStore::open_in_memory().expect("store"),
        Config::from_env(8008),
    );
    let source = state
        .store
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

    let recent = state
        .store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &article(
                &source.id,
                "recent",
                "Regulator blocks the merger",
                NOW - HOUR_MS,
            ),
            NOW,
        )
        .await
        .expect("insert")
        .expect("new");
    let stale = state
        .store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &article(
                &source.id,
                "stale-one",
                "Old merger news",
                NOW - 25 * HOUR_MS,
            ),
            NOW,
        )
        .await
        .expect("insert")
        .expect("new");

    let enabled = topic(&state, "Mergers", "merger").await;
    let disabled = topic(&state, "Regulators", "regulator").await;
    let ast = serde_json::to_value(query::parse("regulator").expect("parses")).expect("ast");
    assert!(state
        .store
        .update_topic(&disabled, "Regulators", "regulator", &ast, false)
        .await
        .expect("update"));
    // An AST no current `Node` variant deserializes from (a hand edit, or a row
    // written by a future version). It must be skipped, not fail the whole pass.
    let broken = state
        .store
        .create_topic(
            DEFAULT_WORKSPACE_ID,
            "Broken",
            "merger",
            &json!({ "kind": "nonsense" }),
        )
        .await
        .expect("create")
        .expect("unique")
        .id;

    assert_eq!(
        match_topics(&state, NOW).await.expect("the pass succeeds"),
        1,
        "only the enabled topic over the recent article"
    );
    let window = (0, NOW + HOUR_MS);
    let matched = state
        .store
        .topic_match_articles(&enabled, window.0, window.1, 10)
        .await
        .expect("matches");
    let ids: Vec<&str> = matched.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec![recent.id.as_str()]);
    assert!(!ids.contains(&stale.id.as_str()));
    assert_eq!(
        state
            .store
            .topic_match_times(&enabled, window.0, window.1)
            .await
            .expect("times"),
        vec![NOW]
    );
    for silent in [&disabled, &broken] {
        assert!(state
            .store
            .topic_match_times(silent, window.0, window.1)
            .await
            .expect("times")
            .is_empty());
    }

    // Idempotent: the same data a second time records nothing new.
    assert_eq!(match_topics(&state, NOW + 1).await.expect("rerun"), 0);
    assert_eq!(
        state
            .store
            .topic_match_times(&enabled, window.0, window.1)
            .await
            .expect("times")
            .len(),
        1
    );
}
