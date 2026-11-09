use super::*;

#[tokio::test]
async fn deleting_a_source_takes_its_articles_and_empties_its_stories() {
    let store = store();
    let source = feed(&store).await;
    let item = insert(&store, &source.id, "https://example.test/1", 9, 1_000).await;
    let story = store
        .create_story(DEFAULT_WORKSPACE_ID, &item.id, &[], &[], 1_000)
        .await
        .unwrap();

    assert!(store.delete_source(&source.id).await.unwrap());
    assert!(store.get_source(&source.id).await.unwrap().is_none());
    assert!(store.get_article(&item.id).await.unwrap().is_none());
    // The story had exactly one member, so it is not a story any more.
    assert!(store.get_story(&story.id).await.unwrap().is_none());
    let counts = store.health_counts().await.unwrap();
    assert_eq!((counts.articles, counts.stories), (0, 0));
    // Deleting something that is already gone reports it rather than pretending.
    assert!(!store.delete_source(&source.id).await.unwrap());
}

#[tokio::test]
async fn the_two_data_category_purges_delete_exactly_what_they_promise() {
    let store = store();
    let source = feed(&store).await;
    let topic = store
        .create_topic(
            DEFAULT_WORKSPACE_ID,
            "Semiconductor export controls",
            "semiconductor AND export",
            &serde_json::json!({"kind": "term", "value": "semiconductor"}),
        )
        .await
        .unwrap()
        .unwrap();
    let item = store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &article(&source.id, "https://example.test/1", 11, 1_000),
            1_000,
        )
        .await
        .unwrap()
        .unwrap();
    store
        .record_topic_match(DEFAULT_WORKSPACE_ID, &topic.id, &item.id, 1_000)
        .await
        .unwrap();
    store
        .create_brief(DEFAULT_WORKSPACE_ID, BriefTrigger::Manual, &[], 1_000)
        .await
        .unwrap();

    // `news_articles`: the collection goes, the subscriptions and watches stay.
    let purged = store.purge_articles(DEFAULT_WORKSPACE_ID).await.unwrap();
    assert_eq!((purged.articles, purged.briefs), (1, 1));
    let counts = store.health_counts().await.unwrap();
    assert_eq!(counts.articles, 0);
    assert_eq!(counts.sources, 1);
    assert_eq!(counts.topics, 1);
    // The fingerprints went with them; a leftover band row would resurrect a
    // deleted article as a dedupe candidate forever.
    assert!(store
        .simhash_candidates(DEFAULT_WORKSPACE_ID, 11, 0)
        .await
        .unwrap()
        .is_empty());

    // `news_sources`: the subscriptions and watches go, whatever is left of the
    // archive stays.
    store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &article(&source.id, "https://example.test/kept", 12, 2_000),
            2_000,
        )
        .await
        .unwrap()
        .unwrap();
    let purged = store
        .purge_sources_and_topics(DEFAULT_WORKSPACE_ID)
        .await
        .unwrap();
    assert_eq!((purged.sources, purged.topics), (1, 1));
    let counts = store.health_counts().await.unwrap();
    assert_eq!((counts.sources, counts.topics, counts.articles), (0, 0, 1));
}

#[tokio::test]
async fn article_filters_narrow_and_archived_rows_leave_the_feed() {
    let store = store();
    let source = feed(&store).await;
    let mut ids = Vec::new();
    for (n, canonical) in ["https://a.test/1", "https://a.test/2", "https://a.test/3"]
        .into_iter()
        .enumerate()
    {
        ids.push(
            store
                .insert_article(
                    DEFAULT_WORKSPACE_ID,
                    &article(&source.id, canonical, n as u64, 1_000 + n as i64),
                    1_000,
                )
                .await
                .unwrap()
                .unwrap()
                .id,
        );
    }
    store
        .set_article_mark(&ids[0], ArticleMark::Read, true, 2_000)
        .await
        .unwrap();
    store
        .set_article_mark(&ids[1], ArticleMark::Archived, true, 2_000)
        .await
        .unwrap();
    store
        .set_article_mark(&ids[2], ArticleMark::Saved, true, 2_000)
        .await
        .unwrap();

    let feed_view = store
        .list_articles(DEFAULT_WORKSPACE_ID, &ArticleQuery::default())
        .await
        .unwrap();
    assert_eq!(feed_view.len(), 2, "the archived one is out of the feed");
    assert!(feed_view.iter().all(|a| !a.is_archived()));
    // Newest published first, and stable on ties.
    assert!(feed_view[0].published_at >= feed_view[1].published_at);

    let unread = store
        .list_articles(
            DEFAULT_WORKSPACE_ID,
            &ArticleQuery {
                unread: Some(true),
                ..ArticleQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].id, ids[2]);

    let saved = store
        .list_articles(
            DEFAULT_WORKSPACE_ID,
            &ArticleQuery {
                saved: Some(true),
                ..ArticleQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert!(saved[0].is_saved());

    // Clearing a mark puts the row back.
    store
        .set_article_mark(&ids[1], ArticleMark::Archived, false, 3_000)
        .await
        .unwrap();
    assert_eq!(
        store
            .list_articles(DEFAULT_WORKSPACE_ID, &ArticleQuery::default())
            .await
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn retention_prunes_old_articles_but_never_a_saved_one() {
    let store = store();
    let source = feed(&store).await;
    let old = store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &article(&source.id, "https://a.test/old", 1, 1_000),
            1_000,
        )
        .await
        .unwrap()
        .unwrap();
    let kept = store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &article(&source.id, "https://a.test/saved", 2, 1_000),
            1_000,
        )
        .await
        .unwrap()
        .unwrap();
    store
        .set_article_mark(&kept.id, ArticleMark::Saved, true, 1_500)
        .await
        .unwrap();

    assert_eq!(
        store
            .prune_articles(DEFAULT_WORKSPACE_ID, 5_000)
            .await
            .unwrap(),
        1
    );
    assert!(store.get_article(&old.id).await.unwrap().is_none());
    assert!(store.get_article(&kept.id).await.unwrap().is_some());
}

#[tokio::test]
async fn a_watch_is_unique_by_name_and_its_matches_feed_the_baseline() {
    let store = store();
    let source = feed(&store).await;
    let ast = serde_json::json!({"kind": "term", "value": "merger"});
    let topic = store
        .create_topic(DEFAULT_WORKSPACE_ID, "Mergers", "merger", &ast)
        .await
        .unwrap()
        .unwrap();
    assert!(store
        .create_topic(DEFAULT_WORKSPACE_ID, "Mergers", "other", &ast)
        .await
        .unwrap()
        .is_none());

    let item = store
        .insert_article(
            DEFAULT_WORKSPACE_ID,
            &article(&source.id, "https://a.test/1", 4, 1_000),
            1_000,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(store
        .record_topic_match(DEFAULT_WORKSPACE_ID, &topic.id, &item.id, 1_000)
        .await
        .unwrap());
    // Re-running the matcher over an article it already matched must not
    // double-count it into the burst baseline.
    assert!(!store
        .record_topic_match(DEFAULT_WORKSPACE_ID, &topic.id, &item.id, 1_100)
        .await
        .unwrap());
    assert_eq!(
        store.topic_match_times(&topic.id, 0, 10_000).await.unwrap(),
        vec![1_000]
    );
    assert_eq!(
        store
            .topic_match_articles(&topic.id, 0, 10_000, 10)
            .await
            .unwrap()
            .len(),
        1
    );

    // The cooldown is the latest burst row, not a column.
    assert!(store.last_burst(&topic.id).await.unwrap().is_none());
    let burst = store
        .record_burst(&TopicBurst {
            id: String::new(),
            workspace_id: DEFAULT_WORKSPACE_ID.into(),
            topic_id: topic.id.clone(),
            z_score: 4.7,
            count: 9,
            baseline_mean: 1.2,
            baseline_stdev: 1.6,
            hour_of_day: 14,
            article_ids: vec![item.id.clone()],
            detected_at: 9_000,
        })
        .await
        .unwrap();
    assert!(burst.id.starts_with(ID_BURST));
    let latest = store.last_burst(&topic.id).await.unwrap().unwrap();
    assert_eq!(latest.article_ids, vec![item.id]);
    assert!((latest.z_score - 4.7).abs() < 1e-9);

    assert!(store.delete_topic(&topic.id).await.unwrap());
    assert!(store.last_burst(&topic.id).await.unwrap().is_none());
    assert!(store
        .topic_match_times(&topic.id, 0, 10_000)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn settings_default_until_saved_and_survive_a_round_trip() {
    let store = store();
    assert_eq!(
        store.get_settings(DEFAULT_WORKSPACE_ID).await.unwrap(),
        NewsSettings::default()
    );
    let settings = NewsSettings {
        brief_time: Some("07:30".into()),
        brief_timezone: Some("Europe/London".into()),
        dedupe: DedupeAggressiveness::Aggressive,
        item_cap: 40,
        ..NewsSettings::default()
    };
    store
        .save_settings(DEFAULT_WORKSPACE_ID, &settings)
        .await
        .unwrap();
    assert_eq!(
        store.get_settings(DEFAULT_WORKSPACE_ID).await.unwrap(),
        settings
    );
}

#[tokio::test]
async fn a_corrupt_settings_blob_degrades_to_defaults_instead_of_stopping_the_poll_loop() {
    let store = store();
    {
        let conn = store.conn.lock().await;
        conn.execute(
            "INSERT INTO settings (workspace_id, json, updated_at) VALUES (?1, '{oh no', 0)",
            params![DEFAULT_WORKSPACE_ID],
        )
        .unwrap();
    }
    assert_eq!(
        store.get_settings(DEFAULT_WORKSPACE_ID).await.unwrap(),
        NewsSettings::default()
    );
}

#[tokio::test]
async fn briefs_list_newest_first() {
    let store = store();
    let items = vec![BriefItem {
        story_id: "st_1".into(),
        title: "Regulator opens inquiry".into(),
        summary: "Two sentences.".into(),
        sources: vec![BriefSource {
            article_id: "ar_1".into(),
            source: "Reuters".into(),
            url: "https://a.test/1".into(),
        }],
    }];
    store
        .create_brief(DEFAULT_WORKSPACE_ID, BriefTrigger::Scheduled, &items, 1_000)
        .await
        .unwrap();
    let newest = store
        .create_brief(DEFAULT_WORKSPACE_ID, BriefTrigger::Manual, &items, 2_000)
        .await
        .unwrap();
    assert_eq!(newest.story_count, 1);
    assert_eq!(newest.article_count, 1);
    assert_eq!(newest.trigger, BriefTrigger::Manual);

    let listed = store.list_briefs(DEFAULT_WORKSPACE_ID, 10).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, newest.id);
    assert_eq!(
        store
            .latest_brief(DEFAULT_WORKSPACE_ID)
            .await
            .unwrap()
            .unwrap()
            .id,
        newest.id
    );
    assert_eq!(listed[0].items[0].sources[0].source, "Reuters");
}

#[test]
fn bands_partition_the_hash_without_losing_a_bit() {
    let hash = 0x0123_4567_89AB_CDEF_u64;
    let bands = simhash_bands(hash);
    assert_eq!(bands, [0xCDEF, 0x89AB, 0x4567, 0x0123]);
    let rebuilt = bands
        .iter()
        .enumerate()
        .fold(0_u64, |acc, (i, b)| acc | ((*b as u64) << (16 * i)));
    assert_eq!(rebuilt, hash);
}
