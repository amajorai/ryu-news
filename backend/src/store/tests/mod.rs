use super::*;

fn store() -> NewsStore {
    NewsStore::open_in_memory().expect("in-memory store")
}

async fn feed(store: &NewsStore) -> Source {
    store
        .create_source(
            DEFAULT_WORKSPACE_ID,
            &NewSource {
                title: "Reuters".into(),
                feed_url: "https://example.test/feed.xml".into(),
                site_url: None,
                kind: SourceKind::Rss,
            },
        )
        .await
        .unwrap()
        .expect("a fresh feed url subscribes")
}

/// Insert an article and return it, panicking on the duplicate case — the tests
/// that care about duplicates check `insert_article` directly.
async fn insert(
    store: &NewsStore,
    source_id: &str,
    canonical: &str,
    hash: u64,
    at: i64,
) -> Article {
    store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &article(source_id, canonical, hash, at),
            at,
        )
        .await
        .unwrap()
        .expect("a fresh canonical url inserts")
}

fn article(source_id: &str, canonical: &str, simhash: u64, published_at: i64) -> NewArticle {
    NewArticle {
        source_id: source_id.to_string(),
        guid: None,
        url: format!("{canonical}?utm_source=x"),
        canonical_url: canonical.to_string(),
        title: "Regulator opens inquiry into the merger".into(),
        author: None,
        summary: None,
        content: None,
        published_at,
        simhash,
    }
}

/// The one test that actually EXECUTES the DDL. `cargo check` cannot: a typo in
/// `V1_DDL` is a string literal that compiles perfectly and then panics on the
/// first real `open()`. Run this before anything else in this crate.
#[tokio::test]
async fn migrations_apply_on_a_fresh_db_and_seed_the_default_workspace() {
    let store = store();
    let workspaces = store.list_workspaces().await.unwrap();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].id, DEFAULT_WORKSPACE_ID);
    assert_eq!(workspaces[0].name, DEFAULT_WORKSPACE_NAME);
    // Every table is reachable, which is the part `cargo check` cannot tell you.
    let counts = store.health_counts().await.unwrap();
    assert_eq!(counts.articles, 0);
    assert_eq!(counts.briefs, 0);
}

/// `migrate()` runs on EVERY open, so an arm that can fail does not fail once —
/// it refuses to boot the sidecar forever, on exactly the databases the fix was
/// meant to repair. This replays v1 over a database that already holds data (by
/// resetting `user_version`, which is what an interrupted migration leaves
/// behind) and asserts both that it succeeds and that it destroys nothing.
#[tokio::test]
async fn migrating_again_over_a_populated_db_is_a_no_op() {
    let conn = Connection::open_in_memory().unwrap();
    NewsStore::prepare(&conn).unwrap();
    conn.execute(
        "INSERT INTO sources
           (id, workspace_id, title, feed_url, site_url, kind, enabled, consecutive_failures,
            created_at)
         VALUES ('src_1', 'default', 'Reuters', 'https://example.test/f', NULL, 'rss', 1, 0, 0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO articles
           (id, workspace_id, source_id, url, canonical_url, title, published_at, fetched_at,
            simhash)
         VALUES ('ar_1', 'default', 'src_1', 'https://example.test/a?utm_source=x',
                 'https://example.test/a', 'Headline', 0, 0, 7)",
        [],
    )
    .unwrap();

    conn.pragma_update(None, "user_version", 0).unwrap();
    NewsStore::migrate(&conn).expect("re-running the v1 arm must not fail");

    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let sources: i64 = conn
        .query_row("SELECT COUNT(*) FROM sources", [], |r| r.get(0))
        .unwrap();
    let articles: i64 = conn
        .query_row("SELECT COUNT(*) FROM articles", [], |r| r.get(0))
        .unwrap();
    assert_eq!((sources, articles), (1, 1));
    // The seed is `INSERT OR IGNORE`, so replaying it does not duplicate the
    // default workspace either.
    let workspaces: i64 = conn
        .query_row("SELECT COUNT(*) FROM workspaces", [], |r| r.get(0))
        .unwrap();
    assert_eq!(workspaces, 1);
}

#[tokio::test]
async fn subscribing_twice_to_one_feed_is_a_no_op_not_a_duplicate() {
    let store = store();
    let first = feed(&store).await;
    let again = store
        .create_source(
            DEFAULT_WORKSPACE_ID,
            &NewSource {
                title: "Reuters (again)".into(),
                feed_url: "https://example.test/feed.xml".into(),
                site_url: None,
                kind: SourceKind::Rss,
            },
        )
        .await
        .unwrap();
    assert!(
        again.is_none(),
        "the UNIQUE index must swallow the re-import"
    );
    assert_eq!(
        store
            .list_sources(DEFAULT_WORKSPACE_ID)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(first.title, "Reuters");
}

#[tokio::test]
async fn the_same_canonical_url_lands_once_however_it_was_reached() {
    let store = store();
    let source = feed(&store).await;
    let inserted = store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &article(&source.id, "https://example.test/a", 0xDEAD_BEEF, 1_000),
            1_000,
        )
        .await
        .unwrap();
    assert!(inserted.is_some());
    let duplicate = store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &article(&source.id, "https://example.test/a", 0xDEAD_BEEF, 2_000),
            2_000,
        )
        .await
        .unwrap();
    assert!(duplicate.is_none(), "the conflict must report, not raise");
}

/// The half of the hashes with the high bit set are the ones a `try_into()`
/// would lose, so this pins the reinterpret round trip specifically.
#[tokio::test]
async fn a_simhash_with_the_high_bit_set_survives_the_round_trip() {
    let store = store();
    let source = feed(&store).await;
    let hash = 0xFFFF_FFFF_FFFF_FFF0_u64;
    let inserted = store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &article(&source.id, "https://example.test/high", hash, 1_000),
            1_000,
        )
        .await
        .unwrap()
        .unwrap();
    let read_back = store.get_article(&inserted.id).await.unwrap().unwrap();
    assert_eq!(read_back.simhash, hash);

    // And it is findable through its own bands, which is what makes near-dupe
    // detection a probe rather than a scan.
    let candidates = store
        .simhash_candidates(DEFAULT_WORKSPACE_ID, hash, 0)
        .await
        .unwrap();
    assert_eq!(candidates, vec![(inserted.id, hash)]);
}

#[tokio::test]
async fn band_probe_finds_a_near_duplicate_and_ignores_an_unrelated_hash() {
    let store = store();
    let source = feed(&store).await;
    let original = 0x0123_4567_89AB_CDEF_u64;
    // One bit away: three of the four bands still match exactly.
    let near = original ^ 1;
    let unrelated = 0xFFFF_0000_FFFF_0000_u64;
    for (canonical, hash) in [
        ("https://example.test/1", original),
        ("https://example.test/2", unrelated),
    ] {
        store
            .insert_article(
                DEFAULT_WORKSPACE_ID,
                &article(&source.id, canonical, hash, 1_000),
                1_000,
            )
            .await
            .unwrap()
            .unwrap();
    }
    let candidates = store
        .simhash_candidates(DEFAULT_WORKSPACE_ID, near, 0)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].1, original);
}

#[tokio::test]
async fn a_failed_fetch_backs_the_source_off_and_surfaces_it_as_failing() {
    let store = store();
    let source = feed(&store).await;
    for _ in 0..FAILING_AFTER_CONSECUTIVE_FAILURES {
        assert!(store
            .record_source_failure(&source.id, 10_000, "connection refused")
            .await
            .unwrap());
    }
    let after = store.get_source(&source.id).await.unwrap().unwrap();
    assert_eq!(
        after.consecutive_failures,
        FAILING_AFTER_CONSECUTIVE_FAILURES
    );
    assert_eq!(after.health(), SourceHealth::Failing);
    assert_eq!(
        after.next_fetch_at,
        Some(10_000 + backoff_hours(FAILING_AFTER_CONSECUTIVE_FAILURES) * 3_600_000)
    );
    // …and it is out of the due set until that backoff elapses.
    assert!(store.due_sources(10_001, 10).await.unwrap().is_empty());

    store
        .record_source_success(&source.id, 20_000, Some("W/\"abc\""), None, 20_500)
        .await
        .unwrap();
    let healed = store.get_source(&source.id).await.unwrap().unwrap();
    assert_eq!(healed.consecutive_failures, 0);
    assert_eq!(healed.health(), SourceHealth::Healthy);
    assert_eq!(healed.etag.as_deref(), Some("W/\"abc\""));
    assert!(healed.last_error.is_none());
}

#[tokio::test]
async fn a_new_cluster_is_born_already_announced_and_only_a_new_outlet_is_growth() {
    let store = store();
    let source = feed(&store).await;
    let first = insert(&store, &source.id, "https://example.test/1", 1, 4_000).await;
    let story = store
        .create_story(
            DEFAULT_WORKSPACE_ID,
            &first.id,
            &["a b c".into()],
            &["Regulator".into()],
            5_000,
        )
        .await
        .unwrap();
    store.set_story_followed(&story.id, true).await.unwrap();

    // A cluster that just opened has nothing "developing" about it. The seed at
    // creation is the only thing standing between this and paging on every new
    // single-outlet report.
    assert_eq!(story.source_count, 1);
    assert_eq!(story.notified_source_count, story.source_count);
    assert!(store
        .followed_stories_that_grew(DEFAULT_WORKSPACE_ID)
        .await
        .unwrap()
        .is_empty());

    // A second article from the SAME outlet is not growth — `source_count` is
    // distinct sources, which is the number the manifest's payload reports.
    let same_outlet = insert(&store, &source.id, "https://example.test/2", 2, 4_500).await;
    store
        .assign_story(&same_outlet.id, &story.id)
        .await
        .unwrap();
    let recounted = store.recount_story(&story.id).await.unwrap().unwrap();
    assert_eq!((recounted.article_count, recounted.source_count), (2, 1));
    assert!(store
        .followed_stories_that_grew(DEFAULT_WORKSPACE_ID)
        .await
        .unwrap()
        .is_empty());

    // A second OUTLET is.
    let other_outlet = store
        .create_source(
            DEFAULT_WORKSPACE_ID,
            &NewSource {
                title: "AP".into(),
                feed_url: "https://ap.test/feed.xml".into(),
                site_url: None,
                kind: SourceKind::Rss,
            },
        )
        .await
        .unwrap()
        .unwrap();
    let joined = insert(&store, &other_outlet.id, "https://ap.test/1", 3, 5_500).await;
    store.assign_story(&joined.id, &story.id).await.unwrap();
    store.recount_story(&story.id).await.unwrap();
    let grew = store
        .followed_stories_that_grew(DEFAULT_WORKSPACE_ID)
        .await
        .unwrap();
    assert_eq!(grew.len(), 1);
    assert_eq!(grew[0].source_count, 2);

    // Announcing it clears it, and the CAS ignores a stale re-announce.
    assert!(store.mark_story_notified(&story.id, 2).await.unwrap());
    assert!(!store.mark_story_notified(&story.id, 1).await.unwrap());
    assert!(store
        .followed_stories_that_grew(DEFAULT_WORKSPACE_ID)
        .await
        .unwrap()
        .is_empty());
}

mod at_rest;
mod lifecycle;
