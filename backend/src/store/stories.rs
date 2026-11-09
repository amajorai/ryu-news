use super::*;

// ── Stories ────────────────────────────────────────────────────────────────────

/// Recompute a story's denormalized counts from its members.
///
/// Shared by every path that changes membership so there is exactly ONE writer for
/// `article_count` / `source_count` / `last_seen_at`. `notified_source_count` is
/// deliberately untouched: it records what `story.developing` last announced, and
/// resetting it here would re-announce growth that was already reported.
pub(super) fn recount_story_in(conn: &Connection, story_id: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE stories SET
           article_count = (SELECT COUNT(*) FROM articles WHERE story_id = stories.id),
           source_count  = (SELECT COUNT(DISTINCT source_id) FROM articles
                             WHERE story_id = stories.id),
           last_seen_at  = COALESCE((SELECT MAX(fetched_at) FROM articles
                                      WHERE story_id = stories.id), last_seen_at)
         WHERE id = ?1",
        params![story_id],
    )
}

/// Remove a story that has no members left. A story is its members; one with none
/// is a row that would render as an empty cluster and count toward the feed.
pub(super) fn drop_story_if_empty(conn: &Connection, story_id: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM stories WHERE id = ?1 AND article_count = 0",
        params![story_id],
    )
}

impl NewsStore {
    /// Open a new cluster around its first member, in one transaction.
    ///
    /// The first article is assigned HERE rather than by a follow-up
    /// `assign_story` call, because two invariants depend on the cluster never
    /// existing in a member-less state:
    ///
    /// 1. `notified_source_count` is seeded EQUAL to `source_count`. The manifest
    ///    promises that a new cluster opening for the first time does not fire
    ///    `story.developing`; a cluster created empty and populated afterwards has
    ///    `source_count 1 > notified 0` for as long as it takes the caller to
    ///    remember, and the next poll pages about a single-outlet report.
    /// 2. An empty story is a row that renders as an empty cluster and counts toward
    ///    the feed. Nothing else in this store can produce one, and the delete paths
    ///    remove any story they empty.
    pub async fn create_story(
        &self,
        workspace_id: &str,
        first_article_id: &str,
        centroid_shingles: &[String],
        entities: &[String],
        at: i64,
    ) -> Result<Story> {
        let id = new_id(ID_STORY);
        let shingles = serde_json::to_string(centroid_shingles)?;
        let entities_json = serde_json::to_string(entities)?;
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO stories
               (id, workspace_id, title, summary, centroid_shingles, centroid_member_count,
                entities, article_count, source_count, notified_source_count, followed,
                first_seen_at, last_seen_at)
             VALUES (?1, ?2, NULL, NULL, ?3, 1, ryu_seal('stories.entities', ?1, ?4), 0, 0, 0, 0,
                     ?5, ?5)",
            params![id, workspace_id, shingles, entities_json, at],
        )?;
        tx.execute(
            "UPDATE articles SET story_id = ?2 WHERE id = ?1",
            params![first_article_id, id],
        )?;
        recount_story_in(&tx, &id)?;
        tx.execute(
            "UPDATE stories SET notified_source_count = source_count WHERE id = ?1",
            params![id],
        )?;
        let sql = format!("SELECT {COLS_STORY} FROM stories WHERE id = ?1");
        let story = tx.query_row(&sql, params![id], row_to_story)?;
        tx.commit()?;
        Ok(story)
    }

    pub async fn get_story(&self, id: &str) -> Result<Option<Story>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_STORY} FROM stories WHERE id = ?1");
        Ok(conn.query_row(&sql, params![id], row_to_story).optional()?)
    }

    /// Stories for the feed view, newest activity first.
    pub async fn list_stories(
        &self,
        workspace_id: &str,
        since: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Story>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_STORY} FROM stories
              WHERE workspace_id = ?1 AND last_seen_at >= ?2
              ORDER BY last_seen_at DESC, id ASC
              LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                workspace_id,
                since.unwrap_or(i64::MIN),
                clamp_limit(Some(limit)) as i64
            ],
            row_to_story,
        )?;
        Ok(collect_readable(rows)?)
    }

    /// The clusterer's join candidates: clusters last touched inside the window.
    ///
    /// The order is fixed IN SQL (`last_seen_at DESC, id ASC`) rather than sorted by
    /// the caller, because replay-stability is a requirement — an article must land
    /// in the same cluster on a re-run — and a sort applied later is one refactor
    /// away from a `HashMap` iteration deciding a cluster.
    pub async fn candidate_stories(&self, workspace_id: &str, since: i64) -> Result<Vec<Story>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_STORY} FROM stories
              WHERE workspace_id = ?1 AND last_seen_at >= ?2
              ORDER BY last_seen_at DESC, id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![workspace_id, since], row_to_story)?;
        Ok(collect_readable(rows)?)
    }

    /// Grow a cluster's centroid with a new member's shingles and entities.
    ///
    /// The caller passes the merged sets and the new member count; the freeze rule
    /// (`centroid_member_count >= settings.centroid_k`) is the clusterer's to apply,
    /// because it is the thing that owns `centroid_k`.
    pub async fn update_story_centroid(
        &self,
        id: &str,
        centroid_shingles: &[String],
        entities: &[String],
        member_count: i64,
    ) -> Result<bool> {
        let shingles = serde_json::to_string(centroid_shingles)?;
        let entities_json = serde_json::to_string(entities)?;
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE stories
                SET centroid_shingles = ?2, entities = ryu_seal('stories.entities', ?1, ?3),
                    centroid_member_count = ?4
              WHERE id = ?1",
            params![id, shingles, entities_json, member_count],
        )?;
        Ok(n > 0)
    }

    /// Recompute one story's counts and return it as it now stands.
    pub async fn recount_story(&self, id: &str) -> Result<Option<Story>> {
        let conn = self.conn.lock().await;
        recount_story_in(&conn, id)?;
        let sql = format!("SELECT {COLS_STORY} FROM stories WHERE id = ?1");
        Ok(conn.query_row(&sql, params![id], row_to_story).optional()?)
    }

    /// The model-written headline and two sentences. Either may be `None` to leave
    /// that half alone.
    pub async fn set_story_prose(
        &self,
        id: &str,
        title: Option<&str>,
        summary: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE stories
                SET title = COALESCE(ryu_seal('stories.title', ?1, ?2), title),
                    summary = COALESCE(ryu_seal('stories.summary', ?1, ?3), summary)
              WHERE id = ?1",
            params![id, title, summary],
        )?;
        Ok(n > 0)
    }

    pub async fn set_story_followed(&self, id: &str, followed: bool) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE stories SET followed = ?2 WHERE id = ?1",
            params![id, followed],
        )?;
        Ok(n > 0)
    }

    /// Followed stories that have gained outlets since the last `story.developing`.
    ///
    /// `source_count > notified_source_count` is the whole predicate, which is why
    /// the seed at creation matters: it is the only thing separating "grew" from
    /// "was born".
    pub async fn followed_stories_that_grew(&self, workspace_id: &str) -> Result<Vec<Story>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_STORY} FROM stories
              WHERE workspace_id = ?1 AND followed = 1 AND source_count > notified_source_count
              ORDER BY last_seen_at DESC, id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![workspace_id], row_to_story)?;
        Ok(collect_readable(rows)?)
    }

    /// Record that growth up to `source_count` has been announced.
    ///
    /// A compare-and-swap on the value the emitter actually reported, not a blind
    /// `= source_count`: if the cluster grew again between the read and this write,
    /// the next poll must still see the newer growth rather than swallowing it.
    pub async fn mark_story_notified(&self, id: &str, notified_source_count: i64) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE stories SET notified_source_count = ?2
              WHERE id = ?1 AND notified_source_count < ?2",
            params![id, notified_source_count],
        )?;
        Ok(n > 0)
    }
}
