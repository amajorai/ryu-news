//! The pipeline, in one place, so the HTTP surface and the MCP server cannot drift.
//!
//! One ingest pass per source:
//!
//! ```text
//! fetch (conditional GET) → parse → canonicalize → dedupe → cluster → match topics
//! ```
//!
//! Every step but the fetch is offline and deterministic. A model is involved in
//! exactly two places, both of them prose: a neutral title for a story eight outlets
//! headlined eight ways, and the brief itself. Nothing a model returns decides what is
//! a duplicate, what belongs to which story, what matched a topic, or what is
//! breaking — those are all arithmetic, and they replay.

use anyhow::{anyhow, Result};

use crate::{
    burst, canon, cluster, extract, feed,
    models::{
        Article, ArticleQuery, Brief, BriefItem, BriefSource, BriefTrigger,
        HeadlineSnapshot, NewArticle, SnapshotItem, Source, Topic, TopicBurst,
    },
    query, rank, simhash,
    state::{AppState, BRIEF_MODEL_PREF_KEY, EVENT_BRIEF_READY, EVENT_STORY_DEVELOPING, EVENT_TOPIC_BREAKING},
    store::NewsStore,
    text,
};

/// How far back the SimHash duplicate probe looks.
///
/// Wire copy is re-syndicated for a day or two, not a month, and an unbounded probe
/// would compare every new article against the whole archive.
pub const DEDUPE_WINDOW_HOURS: i64 = 96;

/// Articles carried in the KV headline snapshot.
pub const SNAPSHOT_SIZE: usize = 40;

/// How long the hook may treat the snapshot as usable.
pub const SNAPSHOT_TTL_SECS: u64 = 3_600;

/// Stories summarized in one brief.
pub const BRIEF_STORY_LIMIT: usize = 12;

const HOUR_MS: i64 = 3_600_000;

/// What one ingest pass did, per source.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct IngestReport {
    pub sources_polled: usize,
    pub articles_new: usize,
    pub duplicates: usize,
    pub stories_opened: usize,
    pub stories_joined: usize,
    pub topic_matches: usize,
    pub bursts: usize,
    pub failures: usize,
}

/// Poll every source that is due, ingest what comes back, and run the derived passes.
pub async fn poll_due_sources(state: &AppState, now: i64) -> Result<IngestReport> {
    let mut report = IngestReport::default();
    let due = state
        .store
        .due_sources(now, state.config.poll_batch_size)
        .await?;
    for source in due {
        report.sources_polled += 1;
        match ingest_source(state, &source, now).await {
            Ok(one) => {
                report.articles_new += one.articles_new;
                report.duplicates += one.duplicates;
            }
            Err(err) => {
                report.failures += 1;
                // A failing source is recorded and backed off, never retried in a
                // tight loop and never allowed to fail the whole pass — one dead feed
                // must not stop the other forty.
                tracing::warn!(source = %source.id, error = %err, "news: source poll failed");
                let _ = state
                    .store
                    .record_source_failure(&source.id, now, &err.to_string())
                    .await;
            }
        }
    }

    let clustered = cluster_pending(state, now).await.unwrap_or_default();
    report.stories_opened = clustered.0;
    report.stories_joined = clustered.1;
    report.topic_matches = match_topics(state, now).await.unwrap_or(0);
    report.bursts = detect_bursts(state, now).await.unwrap_or(0);
    let _ = publish_snapshot(state, now).await;
    let _ = raise_developing_stories(state).await;
    Ok(report)
}

/// Fetch and ingest one source.
async fn ingest_source(state: &AppState, source: &Source, now: i64) -> Result<IngestReport> {
    let mut report = IngestReport::default();

    let mut request = state.http.get(&source.feed_url);
    // Conditional GET. Most feeds change a few times a day and are polled far more
    // often than that; without this the app re-downloads and re-parses the same bytes
    // dozens of times a day per source, for every user.
    if let Some(etag) = source.etag.as_deref() {
        request = request.header("if-none-match", etag);
    }
    if let Some(modified) = source.last_modified.as_deref() {
        request = request.header("if-modified-since", modified);
    }
    let response = request.send().await?;

    let status = response.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        state
            .store
            .record_source_success(&source.id, now, source.etag.as_deref(), source.last_modified.as_deref(), now + HOUR_MS)
            .await?;
        return Ok(report);
    }
    if !status.is_success() {
        return Err(anyhow!("the source answered {status}"));
    }
    let etag = header(&response, "etag");
    let last_modified = header(&response, "last-modified");
    let body = response.text().await?;

    let parsed = feed::parse_feed(&body, now)?;
    for item in parsed.items {
        let canonical = canon::canonicalize(&item.url);
        // Content first, falling back to the summary: a feed that ships only a
        // description still has to fingerprint and cluster on something.
        let content = item
            .content
            .clone()
            .or_else(|| item.summary.clone())
            .unwrap_or_default();
        let fingerprint = simhash::simhash(&format!("{} {content}", item.title));

        let duplicate_of = find_duplicate(&state.store, &source.workspace_id, fingerprint, now).await;
        let new = NewArticle {
            source_id: source.id.clone(),
            guid: item.guid.clone(),
            url: item.url.clone(),
            canonical_url: canonical,
            title: item.title.clone(),
            author: item.author.clone(),
            summary: item.summary.clone(),
            content: Some(content),
            published_at: item.published_at,
            simhash: fingerprint,
        };
        // `insert_article` upserts on (source, guid/canonical url) and returns `None`
        // when the row already existed, so re-polling a feed is free.
        if let Some(article) = state.store.insert_article(&source.workspace_id, &new, now).await? {
            report.articles_new += 1;
            if duplicate_of.is_some() {
                report.duplicates += 1;
                let _ = state
                    .store
                    .set_duplicate_of(&article.id, duplicate_of.as_deref())
                    .await;
            }
        }
    }

    state
        .store
        .record_source_success(
            &source.id,
            now,
            etag.as_deref(),
            last_modified.as_deref(),
            now + HOUR_MS,
        )
        .await?;
    Ok(report)
}

fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// The near-duplicate probe: band lookup, then a real Hamming check.
///
/// The band index narrows the candidate set to a handful; the distance check is what
/// actually decides. Skipping the second step would collapse anything sharing 16 bits.
async fn find_duplicate(
    store: &NewsStore,
    workspace_id: &str,
    fingerprint: u64,
    now: i64,
) -> Option<String> {
    if fingerprint == 0 {
        return None;
    }
    let since = now - DEDUPE_WINDOW_HOURS * HOUR_MS;
    let candidates = store
        .simhash_candidates(workspace_id, fingerprint, since)
        .await
        .ok()?;
    candidates
        .into_iter()
        .find(|(_, other)| simhash::is_near_duplicate(fingerprint, *other))
        .map(|(id, _)| id)
}

/// Cluster everything not yet assigned to a story. Returns (opened, joined).
pub async fn cluster_pending(state: &AppState, now: i64) -> Result<(usize, usize)> {
    let workspaces = state.store.list_workspaces().await?;
    let (mut opened, mut joined) = (0usize, 0usize);
    for workspace in workspaces {
        let pending = state.store.unclustered_articles(&workspace.id, 200).await?;
        for article in pending {
            // A known duplicate inherits its original's story rather than clustering
            // on its own: two copies of one wire story are one story, and letting the
            // copy cluster independently is how a story ends up listed twice.
            if let Some(original) = article.duplicate_of.as_deref() {
                if let Ok(Some(other)) = state.store.get_article(original).await {
                    if let Some(story_id) = other.story_id {
                        let _ = state.store.assign_story(&article.id, &story_id).await;
                        continue;
                    }
                }
            }

            let features = cluster::features(&article.title, article.content.as_deref().unwrap_or(""));
            let stories = state
                .store
                .candidate_stories(&workspace.id, now - cluster::WINDOW_HOURS * HOUR_MS)
                .await?;
            let candidates: Vec<cluster::Candidate> = stories
                .iter()
                .map(|story| cluster::Candidate {
                    id: story.id.clone(),
                    shingles: story.centroid_shingles.iter().cloned().collect(),
                    entities: story.entities.iter().cloned().collect(),
                    title_tokens: text::query_tokens(&text::normalize(
                        story.title.as_deref().unwrap_or(""),
                    ))
                    .into_iter()
                    .collect(),
                    last_seen_at: story.last_seen_at,
                    member_count: story.article_count,
                })
                .collect();

            match cluster::assign(&features, &candidates, now) {
                cluster::Assignment::Join { story_id, .. } => {
                    state.store.assign_story(&article.id, &story_id).await?;
                    if let Some(story) = state.store.recount_story(&story_id).await? {
                        let (centroid, entities) = cluster::fold_centroid(
                            &story.centroid_shingles.iter().cloned().collect(),
                            &story.entities.iter().cloned().collect(),
                            &features,
                            story.centroid_member_count,
                        );
                        let centroid: Vec<String> = centroid.into_iter().collect();
                        let entities: Vec<String> = entities.into_iter().collect();
                        state
                            .store
                            .update_story_centroid(
                                &story_id,
                                &centroid,
                                &entities,
                                story.centroid_member_count + 1,
                            )
                            .await?;
                    }
                    joined += 1;
                }
                cluster::Assignment::Open => {
                    let shingles: Vec<String> = features.shingles.iter().cloned().collect();
                    let entities: Vec<String> = features.entities.iter().cloned().collect();
                    let story = state
                        .store
                        .create_story(&workspace.id, &article.id, &shingles, &entities, now)
                        .await?;
                    state.store.assign_story(&article.id, &story.id).await?;
                    opened += 1;
                }
            }
        }
    }
    Ok((opened, joined))
}

/// Evaluate every enabled topic against recent articles.
pub async fn match_topics(state: &AppState, now: i64) -> Result<usize> {
    let workspaces = state.store.list_workspaces().await?;
    let mut matched = 0usize;
    for workspace in workspaces {
        let topics = state.store.list_topics(&workspace.id).await?;
        if topics.is_empty() {
            continue;
        }
        let recent = state
            .store
            .list_articles(
                &workspace.id,
                &ArticleQuery {
                    since: Some(now - 24 * HOUR_MS),
                    limit: Some(500),
                    ..Default::default()
                },
            )
            .await?;

        // Parse each topic ONCE per pass, not once per article. The AST is stored, so
        // this is a deserialize rather than a parse, but doing it inside the article
        // loop would still be a few hundred needless allocations per topic.
        let compiled: Vec<(&Topic, query::Node)> = topics
            .iter()
            .filter(|t| t.enabled)
            .filter_map(|topic| {
                serde_json::from_value::<query::Node>(topic.ast.clone())
                    .ok()
                    .map(|node| (topic, node))
            })
            .collect();

        for article in &recent {
            let document = query::Document::new(
                &article.title,
                article.content.as_deref().unwrap_or(""),
                "",
                article.author.as_deref().unwrap_or(""),
                &article.url,
            );
            for (topic, node) in &compiled {
                if node.matches(&document) {
                    if state
                        .store
                        .record_topic_match(&workspace.id, &topic.id, &article.id, now)
                        .await
                        .unwrap_or(false)
                    {
                        matched += 1;
                    }
                }
            }
        }
    }
    Ok(matched)
}

/// Run the burst test for every topic and raise `topic.breaking`.
pub async fn detect_bursts(state: &AppState, now: i64) -> Result<usize> {
    let workspaces = state.store.list_workspaces().await?;
    let mut fired = 0usize;
    for workspace in workspaces {
        let settings = state.store.get_settings(&workspace.id).await?;
        for topic in state.store.list_topics(&workspace.id).await? {
            if !topic.enabled {
                continue;
            }
            let hour_of_day = hour_of_day(now);
            let count = state
                .store
                .topic_match_times(&topic.id, now - HOUR_MS, now)
                .await?
                .len() as i64;

            // The baseline is built from the SAME hour of day over the trailing week.
            // A flat weekly mean makes every weekday morning a three-sigma event.
            let mut samples = Vec::new();
            for day in 1..=7i64 {
                let centre = now - day * 24 * HOUR_MS;
                let times = state
                    .store
                    .topic_match_times(&topic.id, centre - HOUR_MS / 2, centre + HOUR_MS / 2)
                    .await?;
                samples.push(times.len() as i64);
            }

            let last = state.store.last_burst(&topic.id).await?;
            let verdict = burst::evaluate(
                count,
                &samples,
                hour_of_day,
                last.as_ref().map(|b| b.detected_at),
                now,
                settings.burst_z_threshold,
            );
            if let burst::Verdict::Burst(stats) = verdict {
                // Fetched before the row is written: the burst record carries the
                // articles that caused it, which is what makes the alert explicable
                // after the fact rather than just a number.
                let articles: Vec<String> = state
                    .store
                    .topic_match_articles(&topic.id, now - HOUR_MS, now, 10)
                    .await?
                    .into_iter()
                    .map(|a| a.id)
                    .collect();
                let recorded = state
                    .store
                    .record_burst(&TopicBurst {
                        id: String::new(),
                        workspace_id: workspace.id.clone(),
                        topic_id: topic.id.clone(),
                        z_score: stats.z_score,
                        count: stats.count,
                        baseline_mean: stats.baseline_mean,
                        baseline_stdev: stats.baseline_stdev,
                        hour_of_day: stats.hour_of_day,
                        article_ids: articles.clone(),
                        detected_at: now,
                    })
                    .await?;
                state
                    .events
                    .emit(
                        EVENT_TOPIC_BREAKING,
                        serde_json::json!({
                            "topic_id": topic.id,
                            "topic": topic.name,
                            "count": stats.count,
                            "z_score": stats.z_score,
                            "baseline_mean": stats.baseline_mean,
                            "hour_of_day": stats.hour_of_day,
                            "article_ids": articles,
                            "burst_id": recorded.id,
                            "detected_at": now,
                        }),
                    )
                    .await;
                fired += 1;
            }
        }
    }
    Ok(fired)
}

/// Raise `story.developing` for followed stories that gained sources.
async fn raise_developing_stories(state: &AppState) -> Result<()> {
    for workspace in state.store.list_workspaces().await? {
        for story in state.store.followed_stories_that_grew(&workspace.id).await? {
            state
                .events
                .emit(
                    EVENT_STORY_DEVELOPING,
                    serde_json::json!({
                        "story_id": story.id,
                        "title": story.title,
                        "source_count": story.source_count,
                        "previous_source_count": story.notified_source_count,
                        "article_count": story.article_count,
                    }),
                )
                .await;
            let _ = state
                .store
                .mark_story_notified(&story.id, story.source_count)
                .await;
        }
    }
    Ok(())
}

/// Publish the ranked headline snapshot the "Ground in news" hook reads.
pub async fn publish_snapshot(state: &AppState, now: i64) -> Result<()> {
    let Some(host) = state.host.as_ref() else {
        return Ok(());
    };
    let workspaces = state.store.list_workspaces().await?;
    let Some(workspace) = workspaces.first() else {
        return Ok(());
    };
    let settings = state.store.get_settings(&workspace.id).await?;
    let articles = state
        .store
        .list_articles(
            &workspace.id,
            &ArticleQuery {
                since: Some(now - 48 * HOUR_MS),
                limit: Some(400),
                ..Default::default()
            },
        )
        .await?;

    let ranked = rank_articles(&articles, now, settings.rank_half_life_hours);
    let items: Vec<SnapshotItem> = ranked
        .into_iter()
        .take(SNAPSHOT_SIZE)
        .map(|(article, _)| SnapshotItem {
            id: article.id.clone(),
            title: article.title.clone(),
            source: article.source_id.clone(),
            url: article.url.clone(),
            published_at: article.published_at.to_string(),
            story_id: article.story_id.clone(),
            source_count: 1,
            // The tokens ride ALONG with the item so the hook does not need a
            // tokenizer of its own — a second implementation in JS would drift from
            // `text.rs` and the two would disagree about what matched.
            tokens: text::snapshot_tokens(&article.title),
        })
        .collect();

    let snapshot = HeadlineSnapshot {
        version: 1,
        generated_at: now.to_string(),
        ttl_secs: SNAPSHOT_TTL_SECS,
        stopwords: text::stopword_list(),
        items,
    };
    host.publish_snapshot(&snapshot).await;
    Ok(())
}

/// Poll ONE source now, on demand.
///
/// Shares `ingest_source` with the scheduled pass, so a manual refresh cannot behave
/// differently from an automatic one — the commonest way a "refresh" button ends up
/// lying about what it did.
pub async fn refresh_one(state: &AppState, source: &Source, now: i64) -> Result<IngestReport> {
    let mut report = match ingest_source(state, source, now).await {
        Ok(report) => report,
        Err(err) => {
            state
                .store
                .record_source_failure(&source.id, now, &err.to_string())
                .await?;
            return Err(err);
        }
    };
    let (opened, joined) = cluster_pending(state, now).await.unwrap_or_default();
    report.stories_opened = opened;
    report.stories_joined = joined;
    report.topic_matches = match_topics(state, now).await.unwrap_or(0);
    Ok(report)
}

/// Serialize the source list as OPML.
///
/// Hand-written rather than templated because it is twelve lines and the alternative
/// is another dependency in a lockfile several jobs share. Text is XML-escaped —
/// a feed title containing `&` is common and would otherwise produce a file no reader
/// can open.
#[must_use]
pub fn to_opml(sources: &[Source]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<opml version=\"2.0\">\n  <head><title>Wire</title></head>\n  <body>\n",
    );
    for source in sources {
        out.push_str(&format!(
            "    <outline type=\"rss\" text=\"{}\" title=\"{}\" xmlUrl=\"{}\"{} />\n",
            xml_escape(&source.title),
            xml_escape(&source.title),
            xml_escape(&source.feed_url),
            source
                .site_url
                .as_deref()
                .map(|url| format!(" htmlUrl=\"{}\"", xml_escape(url)))
                .unwrap_or_default(),
        ));
    }
    out.push_str("  </body>\n</opml>\n");
    out
}

fn xml_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Import sources from an OPML document. Returns (added, skipped).
///
/// Skipped rather than failed: an OPML export from another reader routinely contains
/// feeds already subscribed to and outlines that are folders rather than feeds, and
/// refusing the whole file over one of those would make the feature useless.
pub async fn import_opml(state: &AppState, workspace_id: &str, opml: &str) -> Result<(usize, usize)> {
    let (mut added, mut skipped) = (0usize, 0usize);
    for raw in opml.split("<outline").skip(1) {
        let Some(url) = attribute(raw, "xmlUrl") else {
            skipped += 1;
            continue;
        };
        if !canon::is_http_url(&url) {
            skipped += 1;
            continue;
        }
        let title = attribute(raw, "title")
            .or_else(|| attribute(raw, "text"))
            .unwrap_or_else(|| url.clone());
        let created = state
            .store
            .create_source(
                workspace_id,
                &crate::models::NewSource {
                    title,
                    feed_url: url,
                    site_url: attribute(raw, "htmlUrl"),
                    // The format is not known until the first fetch parses it, and
                    // OPML does not reliably say. `Rss` is the placeholder the first
                    // poll overwrites with what the bytes actually were.
                    kind: crate::models::SourceKind::Rss,
                },
            )
            .await?;
        if created.is_some() {
            added += 1;
        } else {
            skipped += 1;
        }
    }
    Ok((added, skipped))
}

/// Read one double-quoted attribute out of an OPML outline fragment.
fn attribute(fragment: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = fragment.find(&needle)? + needle.len();
    let rest = fragment.get(start..)?;
    let end = rest.find('"')?;
    let value = rest.get(..end)?.trim();
    if value.is_empty() {
        return None;
    }
    Some(
        value
            .replace("&quot;", "\"")
            .replace("&gt;", ">")
            .replace("&lt;", "<")
            // Ampersand LAST, or `&amp;lt;` would decode twice.
            .replace("&amp;", "&"),
    )
}

/// Rank articles, highest first, carrying each one's factors.
#[must_use]
pub fn rank_articles(
    articles: &[Article],
    now: i64,
    half_life_hours: f64,
) -> Vec<(Article, rank::Factors)> {
    let inputs: Vec<(String, rank::Input, Article)> = articles
        .iter()
        .map(|article| {
            (
                article.id.clone(),
                rank::Input {
                    published_at: article.published_at,
                    source_count: 1,
                    topic_matches: 0,
                    is_read: article.is_read(),
                },
                article.clone(),
            )
        })
        .collect();
    rank::rank(&inputs, now, half_life_hours)
        .into_iter()
        .map(|(_, factors, article)| (article, factors))
        .collect()
}

/// Write a brief: one entry per recent story, with its sources attached.
///
/// One of the app's exactly two model edges. The raw cluster stays underneath every
/// entry, so a summary can always be checked against what it summarized.
pub async fn generate_brief(state: &AppState, trigger: BriefTrigger, now: i64) -> Result<Brief> {
    let host = state
        .host
        .as_ref()
        .ok_or_else(|| anyhow!(crate::host::no_host_message()))?;
    let workspaces = state.store.list_workspaces().await?;
    let workspace = workspaces
        .first()
        .ok_or_else(|| anyhow!("there is no workspace yet"))?;

    let stories = state
        .store
        .list_stories(&workspace.id, Some(now - 24 * HOUR_MS), BRIEF_STORY_LIMIT)
        .await?;
    if stories.is_empty() {
        return Err(anyhow!("nothing has come in since the last brief"));
    }

    let mut lines = Vec::new();
    for (index, story) in stories.iter().enumerate() {
        lines.push(format!(
            "{}. {} ({} sources)",
            index + 1,
            story.title.as_deref().unwrap_or("(untitled)"),
            story.source_count
        ));
    }
    let system = "You write a short news brief. Reply with JSON only: \
        {\"items\":[{\"index\":1,\"summary\":\"two sentences\",\"why\":\"one sentence on why it \
        matters\"}]}. Summarize ONLY what the headlines say. Do not add facts, do not \
        speculate about consequences that are not stated, and do not editorialize.";
    let prompt = format!("Stories:\n{}", lines.join("\n"));
    let raw = host
        .complete(system, &prompt, Some(BRIEF_MODEL_PREF_KEY))
        .await?;
    let parsed = crate::service::extract_json(&raw)
        .ok_or_else(|| anyhow!("the brief model did not return JSON"))?;

    let prose: std::collections::BTreeMap<usize, (String, String)> = parsed
        .get("items")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let index = item.get("index")?.as_u64()? as usize;
                    let summary = item.get("summary")?.as_str()?.to_owned();
                    let why = item
                        .get("why")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    Some((index, (summary, why)))
                })
                .collect()
        })
        .unwrap_or_default();

    let items: Vec<BriefItem> = stories
        .iter()
        .enumerate()
        .map(|(index, story)| {
            let (summary, why) = prose.get(&(index + 1)).cloned().unwrap_or_default();
            BriefItem {
                story_id: story.id.clone(),
                title: story.title.clone().unwrap_or_else(|| "(untitled)".into()),
                // An entry the model skipped keeps its headline and its sources and
                // simply has no prose. Dropping it would silently shrink the brief.
                // The model's "why it matters" sentence is appended to the
                // summary rather than stored separately: `BriefItem` has one prose
                // field, and a second one that only sometimes exists is a shape every
                // consumer then has to branch on.
                summary: if why.is_empty() {
                    summary
                } else {
                    format!("{summary} {why}")
                },
                sources: Vec::<BriefSource>::new(),
            }
        })
        .collect();

    let brief = state
        .store
        .create_brief(&workspace.id, trigger, &items, now)
        .await?;
    state
        .events
        .emit(
            EVENT_BRIEF_READY,
            serde_json::json!({
                "brief_id": brief.id,
                "story_count": brief.items.len(),
                "trigger": trigger.as_str(),
                "generated_at": now,
            }),
        )
        .await;
    Ok(brief)
}

/// Hour of day, UTC, for the burst baseline bucket.
fn hour_of_day(at: i64) -> i64 {
    (at / HOUR_MS) % 24
}

/// Pull the first balanced JSON object out of a model reply.
///
/// Models wrap JSON in prose and fences no matter how firmly the system prompt says
/// not to. String-aware, so a `}` inside a quoted summary does not end it.
#[must_use]
pub fn extract_json(text: &str) -> Option<serde_json::Value> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(text.get(start..=index)?).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract readable text from an HTML page, for a source with no usable feed.
#[must_use]
pub fn page_text(html: &str) -> String {
    extract::extract_main_text(html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_recovered_from_a_fenced_reply() {
        let raw = "Here you go:\n```json\n{\"items\":[{\"index\":1,\"summary\":\"A.\"}]}\n```";
        let parsed = extract_json(raw).expect("must recover the object");
        assert_eq!(parsed["items"][0]["index"], 1);
    }

    #[test]
    fn a_brace_inside_a_string_does_not_end_the_object() {
        let raw = r#"{"summary": "they used a } brace", "index": 1}"#;
        assert_eq!(extract_json(raw).expect("parses")["index"], 1);
    }

    #[test]
    fn an_unterminated_object_yields_nothing_rather_than_panicking() {
        assert!(extract_json("{\"items\": [").is_none());
        assert!(extract_json("").is_none());
    }

    #[test]
    fn opml_escapes_titles_that_would_otherwise_produce_an_unopenable_file() {
        let sources = vec![Source {
            id: "s1".into(),
            workspace_id: "w".into(),
            title: "Tom & Jerry <news>".into(),
            feed_url: "https://example.test/feed?a=1&b=2".into(),
            site_url: None,
            kind: crate::models::SourceKind::Rss,
            enabled: true,
            etag: None,
            last_modified: None,
            last_fetch_at: None,
            last_success_at: None,
            consecutive_failures: 0,
            next_fetch_at: None,
            last_error: None,
            created_at: 0,
        }];
        let opml = to_opml(&sources);
        assert!(opml.contains("Tom &amp; Jerry &lt;news&gt;"), "{opml}");
        assert!(!opml.contains("Tom & Jerry"), "raw ampersand survived");
    }

    #[test]
    fn an_opml_attribute_decodes_entities_in_the_right_order() {
        // Ampersand last: decoding it first would turn `&amp;lt;` into `<`.
        let fragment = r#" type="rss" title="A &amp;amp; B" xmlUrl="https://x.test/f" "#;
        assert_eq!(attribute(fragment, "title").as_deref(), Some("A &amp; B"));
        assert_eq!(
            attribute(fragment, "xmlUrl").as_deref(),
            Some("https://x.test/f")
        );
        assert!(attribute(fragment, "htmlUrl").is_none());
    }

    #[test]
    fn the_hour_bucket_wraps_at_midnight() {
        assert_eq!(hour_of_day(0), 0);
        assert_eq!(hour_of_day(23 * HOUR_MS), 23);
        assert_eq!(hour_of_day(24 * HOUR_MS), 0);
    }
}
