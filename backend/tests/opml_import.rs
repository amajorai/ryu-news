//! OPML bulk import: what an export from another reader turns into.
//!
//! The parser is a hand-written attribute scan, so each rule below is a branch a
//! refactor could quietly drop: non-feed outlines and non-http schemes are skipped,
//! an already-subscribed feed is skipped rather than duplicated, the title falls back
//! title → text → url, entities decode exactly once, and `htmlUrl` lands in
//! `site_url`.

use ryu_news::models::{NewSource, Source, SourceKind, DEFAULT_WORKSPACE_ID};
use ryu_news::service::import_opml;
use ryu_news::state::{AppState, Config};
use ryu_news::store::NewsStore;

fn state() -> AppState {
    let store = NewsStore::open_in_memory().expect("an in-memory store");
    AppState::new(store, Config::from_env(8008))
}

fn by_feed<'a>(sources: &'a [Source], feed_url: &str) -> Option<&'a Source> {
    sources.iter().find(|s| s.feed_url == feed_url)
}

const OPML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head><title>Export</title></head>
  <body>
    <outline text="Folder">
      <outline type="rss" title="Ars &amp;amp; Friends" text="ignored" xmlUrl="https://ars.test/feed" htmlUrl="https://ars.test/"/>
      <outline type="rss" text="Text Only" xmlUrl="https://text.test/rss"/>
      <outline type="rss" xmlUrl="https://bare.test/atom.xml"/>
      <outline type="rss" title="Evil" xmlUrl="javascript:alert(1)"/>
      <outline type="rss" title="Local" xmlUrl="file:///etc/passwd"/>
      <outline type="rss" title="Already" xmlUrl="https://known.test/feed"/>
    </outline>
  </body>
</opml>"#;

#[tokio::test]
async fn an_opml_export_adds_each_https_feed_and_skips_the_rest() {
    let state = state();
    state
        .store
        .create_source(
            DEFAULT_WORKSPACE_ID,
            &NewSource {
                title: "Known".into(),
                feed_url: "https://known.test/feed".into(),
                site_url: None,
                kind: SourceKind::Rss,
            },
        )
        .await
        .expect("create")
        .expect("a fresh url subscribes");

    let (added, skipped) = import_opml(&state, DEFAULT_WORKSPACE_ID, OPML)
        .await
        .expect("the import runs");
    // Folder (no xmlUrl), javascript:, file: and the already-known feed are skipped.
    assert_eq!((added, skipped), (3, 4));

    let sources = state
        .store
        .list_sources(DEFAULT_WORKSPACE_ID)
        .await
        .expect("list");
    assert_eq!(sources.len(), 4, "3 imported + the 1 already there");
    assert_eq!(
        sources
            .iter()
            .filter(|s| s.feed_url == "https://known.test/feed")
            .count(),
        1,
        "an already-subscribed feed is not duplicated"
    );
    assert!(sources.iter().all(|s| s.feed_url.starts_with("https://")));

    let ars = by_feed(&sources, "https://ars.test/feed").expect("ars imported");
    // `title` wins over `text`, and `&amp;amp;` decodes exactly once to `&amp;`.
    assert_eq!(ars.title, "Ars &amp; Friends");
    assert_eq!(ars.site_url.as_deref(), Some("https://ars.test/"));

    let text_only = by_feed(&sources, "https://text.test/rss").expect("text imported");
    assert_eq!(text_only.title, "Text Only", "falls back to `text`");
    assert_eq!(text_only.site_url, None);

    let bare = by_feed(&sources, "https://bare.test/atom.xml").expect("bare imported");
    assert_eq!(
        bare.title, "https://bare.test/atom.xml",
        "falls back to the url"
    );
}

#[tokio::test]
async fn importing_the_same_file_twice_adds_nothing_the_second_time() {
    let state = state();
    let first = import_opml(&state, DEFAULT_WORKSPACE_ID, OPML)
        .await
        .expect("first import");
    assert_eq!(first, (4, 3));
    let second = import_opml(&state, DEFAULT_WORKSPACE_ID, OPML)
        .await
        .expect("second import");
    assert_eq!(second, (0, 7));
    assert_eq!(
        state
            .store
            .list_sources(DEFAULT_WORKSPACE_ID)
            .await
            .expect("list")
            .len(),
        4
    );
}
