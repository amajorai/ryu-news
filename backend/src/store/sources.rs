use super::*;

// ── Sources ────────────────────────────────────────────────────────────────────

impl NewsStore {
    pub async fn list_sources(&self, workspace_id: &str) -> Result<Vec<Source>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_SOURCE} FROM sources WHERE workspace_id = ?1");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![workspace_id], row_to_source)?;
        // Titles are sealed, so the order is applied after opening them.
        let mut sources = collect_readable(rows)?;
        sources.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
        Ok(sources)
    }

    pub async fn get_source(&self, id: &str) -> Result<Option<Source>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_SOURCE} FROM sources WHERE id = ?1");
        Ok(conn
            .query_row(&sql, params![id], row_to_source)
            .optional()?)
    }

    /// Subscribe. Returns `None` when this workspace is already subscribed to the
    /// feed — the UNIQUE index decides, not a preceding SELECT, so an OPML import of
    /// 200 overlapping feeds is one statement per feed with no race in the middle.
    ///
    /// `next_fetch_at` is seeded to `now` so a newly added source is picked up by
    /// the very next sweep rather than after one full poll interval of silence.
    pub async fn create_source(
        &self,
        workspace_id: &str,
        new_source: &NewSource,
    ) -> Result<Option<Source>> {
        let now = now_ms();
        let conn = self.conn.lock().await;
        let sql = format!(
            "INSERT INTO sources
               (id, workspace_id, title, feed_url, site_url, kind, enabled, etag, last_modified,
                last_fetch_at, last_success_at, consecutive_failures, next_fetch_at, last_error,
                created_at, feed_url_bi)
             VALUES (?1, ?2, ryu_seal('sources.title', ?1, ?3), ryu_seal('sources.feed_url', ?1, ?4),
                     ryu_seal('sources.site_url', ?1, ?5), ?6, 1, NULL, NULL, NULL, NULL, 0, ?7,
                     NULL, ?7, ?8)
             ON CONFLICT DO NOTHING
             RETURNING {COLS_SOURCE}"
        );
        Ok(conn
            .query_row(
                &sql,
                params![
                    new_id(ID_SOURCE),
                    workspace_id,
                    new_source.title.trim(),
                    new_source.feed_url.trim(),
                    new_source.site_url.as_deref(),
                    new_source.kind.as_str(),
                    now,
                    self.feed_token(new_source.feed_url.trim()),
                ],
                row_to_source,
            )
            .optional()?)
    }

    /// The three editable fields, replaced together. Returns `false` when no row
    /// matched so the handler can 404 instead of reporting a successful no-op.
    pub async fn update_source(
        &self,
        id: &str,
        title: &str,
        site_url: Option<&str>,
        enabled: bool,
    ) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE sources SET title = ryu_seal('sources.title', ?1, ?2),
                    site_url = ryu_seal('sources.site_url', ?1, ?3), enabled = ?4 WHERE id = ?1",
            params![id, title.trim(), site_url, enabled],
        )?;
        Ok(n > 0)
    }

    /// Unsubscribe, and take this source's articles with it.
    ///
    /// An explicit ordered cascade in ONE transaction (see the module docs for why
    /// not `ON DELETE CASCADE`). The order is load-bearing: `topic_matches` and
    /// `article_bands` are reached only THROUGH the articles, so they must go before
    /// the articles are removed or they are stranded with no way left to find them.
    /// The stories the articles belonged to are then recounted and the ones left
    /// empty are dropped, because a story with no articles is not a story.
    pub async fn delete_source(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let n = delete_source_in(&tx, id)?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Sources the poll loop may fetch now: enabled, and either never fetched or out
    /// of their backoff. Oldest-due first so a long backlog drains in order rather
    /// than starving whichever source sorts last alphabetically.
    pub async fn due_sources(&self, now: i64, limit: usize) -> Result<Vec<Source>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_SOURCE} FROM sources
              WHERE enabled = 1 AND (next_fetch_at IS NULL OR next_fetch_at <= ?1)
              ORDER BY COALESCE(next_fetch_at, 0) ASC, id ASC
              LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![now, clamp_limit(Some(limit)) as i64], row_to_source)?;
        Ok(collect_readable(rows)?)
    }

    /// Record a successful fetch: clear the failure counter, store the new
    /// conditional-GET validators, and schedule the next poll.
    ///
    /// The validators are written even on a `304 Not Modified`, because a server is
    /// allowed to hand back a fresh `ETag` with one.
    pub async fn record_source_success(
        &self,
        id: &str,
        at: i64,
        etag: Option<&str>,
        last_modified: Option<&str>,
        next_fetch_at: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE sources
                SET last_fetch_at = ?2, last_success_at = ?2, consecutive_failures = 0,
                    etag = ?3, last_modified = ?4, next_fetch_at = ?5, last_error = NULL
              WHERE id = ?1",
            params![id, at, etag, last_modified, next_fetch_at],
        )?;
        Ok(n > 0)
    }

    /// Record a failed fetch and put the source into its next backoff step.
    ///
    /// The counter is incremented IN SQL and returned, then the backoff is derived
    /// from the value that actually landed — a read-then-write would compute the
    /// delay from a count that a concurrent sweep may already have moved.
    pub async fn record_source_failure(&self, id: &str, at: i64, error: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let failures: Option<i64> = tx
            .query_row(
                "UPDATE sources
                    SET last_fetch_at = ?2,
                        consecutive_failures = consecutive_failures + 1,
                        last_error = ryu_seal('sources.last_error', ?1, ?3)
                  WHERE id = ?1
                  RETURNING consecutive_failures",
                params![id, at, error],
                |r| r.get(0),
            )
            .optional()?;
        let Some(failures) = failures else {
            return Ok(false);
        };
        let next = at + backoff_hours(failures) * 3_600_000;
        tx.execute(
            "UPDATE sources SET next_fetch_at = ?2 WHERE id = ?1",
            params![id, next],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

/// The ordered unsubscribe cascade inside a caller's transaction (see
/// [`NewsStore::delete_source`]). Returns the sources rows removed.
pub(super) fn delete_source_in(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
) -> rusqlite::Result<usize> {
    let touched_stories: Vec<String> = tx
        .prepare(
            "SELECT DISTINCT story_id FROM articles
                  WHERE source_id = ?1 AND story_id IS NOT NULL",
        )?
        .query_map(params![id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    tx.execute(
        "DELETE FROM topic_matches WHERE article_id IN
               (SELECT id FROM articles WHERE source_id = ?1)",
        params![id],
    )?;
    tx.execute(
        "DELETE FROM article_bands WHERE article_id IN
               (SELECT id FROM articles WHERE source_id = ?1)",
        params![id],
    )?;
    tx.execute("DELETE FROM articles WHERE source_id = ?1", params![id])?;
    for story_id in &touched_stories {
        recount_story_in(&tx, story_id)?;
        drop_story_if_empty(&tx, story_id)?;
    }
    tx.execute("DELETE FROM sources WHERE id = ?1", params![id])
}
