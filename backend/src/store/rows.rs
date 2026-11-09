use super::*;

// ── Row decoders ───────────────────────────────────────────────────────────────
//
// One decoder per table, each taking an explicit column order that its callers'
// SELECTs must match. Every SELECT in this file names its columns explicitly rather
// than `SELECT *`, so adding a column can never silently shift a positional read.

/// Decode a JSON `TEXT` column into a list, tolerating a corrupt value.
///
/// Deliberately lossy: a story whose `entities` blob got truncated should render
/// with no entities, not blank the whole feed with a 500. The clusterer treats an
/// empty set as "no evidence", which degrades the score rather than corrupting it.
pub(super) fn json_list(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

pub(super) fn row_to_workspace(row: &Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: row.get(0)?,
        name: row.get(1)?,
        created_at: row.get(2)?,
    })
}

pub(super) fn row_to_source(row: &Row<'_>) -> rusqlite::Result<Source> {
    let kind: String = row.get(5)?;
    Ok(Source {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        title: opened(row, 2)?,
        feed_url: opened(row, 3)?,
        site_url: opened_opt(row, 4)?,
        // An unknown value degrades to the default rather than failing the list —
        // see the enum note in `crate::models`.
        kind: SourceKind::parse(&kind).unwrap_or_default(),
        enabled: row.get(6)?,
        etag: row.get(7)?,
        last_modified: row.get(8)?,
        last_fetch_at: row.get(9)?,
        last_success_at: row.get(10)?,
        consecutive_failures: row.get(11)?,
        next_fetch_at: row.get(12)?,
        last_error: opened_opt(row, 13)?,
        created_at: row.get(14)?,
    })
}

pub(super) fn row_to_article(row: &Row<'_>) -> rusqlite::Result<Article> {
    // SQLite INTEGER is signed 64-bit and rusqlite has no `ToSql`/`FromSql` for
    // `u64`, so the hash is stored by REINTERPRETING its bits and read back the same
    // way. `try_into()` here would be wrong in the most misleading possible way: it
    // would work for every hash with a clear high bit and fail for the other half.
    let simhash: i64 = row.get(13)?;
    Ok(Article {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        source_id: row.get(2)?,
        story_id: row.get(3)?,
        guid: row.get(4)?,
        url: row.get(5)?,
        canonical_url: row.get(6)?,
        title: row.get(7)?,
        author: row.get(8)?,
        summary: row.get(9)?,
        content: row.get(10)?,
        published_at: row.get(11)?,
        fetched_at: row.get(12)?,
        simhash: simhash as u64,
        duplicate_of: row.get(14)?,
        read_at: row.get(15)?,
        saved_at: row.get(16)?,
        archived_at: row.get(17)?,
    })
}

pub(super) fn row_to_story(row: &Row<'_>) -> rusqlite::Result<Story> {
    let centroid: String = row.get(4)?;
    let entities: String = opened(row, 6)?;
    Ok(Story {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        title: opened_opt(row, 2)?,
        summary: opened_opt(row, 3)?,
        centroid_shingles: json_list(&centroid),
        centroid_member_count: row.get(5)?,
        entities: json_list(&entities),
        article_count: row.get(7)?,
        source_count: row.get(8)?,
        notified_source_count: row.get(9)?,
        followed: row.get(10)?,
        first_seen_at: row.get(11)?,
        last_seen_at: row.get(12)?,
    })
}

pub(super) fn row_to_topic(row: &Row<'_>) -> rusqlite::Result<Topic> {
    let ast: String = opened(row, 4)?;
    Ok(Topic {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        name: opened(row, 2)?,
        query: opened(row, 3)?,
        // A corrupt AST becomes `null`, which the matcher reads as "matches
        // nothing". The stored query text is still there for the user to re-save,
        // which is the only thing that can actually repair it.
        ast: serde_json::from_str(&ast).unwrap_or(serde_json::Value::Null),
        enabled: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

pub(super) fn row_to_burst(row: &Row<'_>) -> rusqlite::Result<TopicBurst> {
    let article_ids: String = row.get(8)?;
    Ok(TopicBurst {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        topic_id: row.get(2)?,
        z_score: row.get(3)?,
        count: row.get(4)?,
        baseline_mean: row.get(5)?,
        baseline_stdev: row.get(6)?,
        hour_of_day: row.get(7)?,
        article_ids: json_list(&article_ids),
        detected_at: row.get(9)?,
    })
}

pub(super) fn row_to_brief(row: &Row<'_>) -> rusqlite::Result<Brief> {
    let trigger: String = row.get(3)?;
    let items: String = opened(row, 6)?;
    Ok(Brief {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        generated_at: row.get(2)?,
        trigger: BriefTrigger::parse(&trigger).unwrap_or_default(),
        story_count: row.get(4)?,
        article_count: row.get(5)?,
        items: serde_json::from_str(&items).unwrap_or_default(),
    })
}

// Column lists, declared once so a decoder and its SELECTs cannot drift apart.
pub(super) const COLS_WORKSPACE: &str = "id, name, created_at";
pub(super) const COLS_SOURCE: &str = "id, workspace_id, ryu_open('sources.title', id, title), \
                           ryu_open('sources.feed_url', id, feed_url), \
                           ryu_open('sources.site_url', id, site_url), kind, enabled, etag, \
                           last_modified, last_fetch_at, last_success_at, consecutive_failures, \
                           next_fetch_at, ryu_open('sources.last_error', id, last_error), \
                           created_at";
pub(super) const COLS_ARTICLE: &str =
    "id, workspace_id, source_id, story_id, guid, url, canonical_url, \
                            title, author, summary, content, published_at, fetched_at, simhash, \
                            duplicate_of, read_at, saved_at, archived_at";
pub(super) const COLS_STORY: &str = "id, workspace_id, ryu_open('stories.title', id, title), \
                          ryu_open('stories.summary', id, summary), centroid_shingles, \
                          centroid_member_count, ryu_open('stories.entities', id, entities), \
                          article_count, source_count, notified_source_count, followed, \
                          first_seen_at, last_seen_at";
pub(super) const COLS_TOPIC: &str = "id, workspace_id, ryu_open('topics.name', id, name), \
                          ryu_open('topics.query', id, query), ryu_open('topics.ast', id, ast), \
                          enabled, created_at, updated_at";
pub(super) const COLS_BURST: &str =
    "id, workspace_id, topic_id, z_score, match_count, baseline_mean, \
                          baseline_stdev, hour_of_day, article_ids, detected_at";
pub(super) const COLS_BRIEF: &str = "id, workspace_id, generated_at, trigger_kind, story_count, \
                          article_count, ryu_open('briefs.items', id, items)";

/// What a purge removed, so the UI can say "deleted 1,204 articles" rather than
/// "done".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct PurgeCounts {
    pub sources: i64,
    pub articles: i64,
    pub stories: i64,
    pub topics: i64,
    pub briefs: i64,
}
