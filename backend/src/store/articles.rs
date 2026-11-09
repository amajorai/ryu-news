use super::*;

// ── Articles ───────────────────────────────────────────────────────────────────

impl NewsStore {
    /// Insert one freshly fetched article, or report it as already present.
    ///
    /// `ON CONFLICT DO NOTHING` with NO conflict target on purpose: both
    /// `(workspace_id, canonical_url)` and `(source_id, guid)` are grounds to skip,
    /// and naming one of them would turn a hit on the other into a 500 out of the
    /// poll loop. No rows back means it was a duplicate — the index is the sole
    /// arbiter, so a concurrent handler inserting the same URL cannot slip between a
    /// check and an insert.
    ///
    /// The band rows go in the SAME transaction. An article without its bands is
    /// invisible to near-duplicate detection forever after, and nothing later would
    /// notice.
    pub async fn insert_article(
        &self,
        workspace_id: &str,
        article: &NewArticle,
        fetched_at: i64,
    ) -> Result<Option<Article>> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let sql = format!(
            "INSERT INTO articles
               (id, workspace_id, source_id, story_id, guid, url, canonical_url, title, author,
                summary, content, published_at, fetched_at, simhash, duplicate_of, read_at,
                saved_at, archived_at)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     NULL, NULL, NULL, NULL)
             ON CONFLICT DO NOTHING
             RETURNING {COLS_ARTICLE}"
        );
        let inserted = tx
            .query_row(
                &sql,
                params![
                    new_id(ID_ARTICLE),
                    workspace_id,
                    article.source_id,
                    article.guid.as_deref(),
                    article.url,
                    article.canonical_url,
                    article.title,
                    article.author.as_deref(),
                    article.summary.as_deref(),
                    article.content.as_deref(),
                    article.published_at,
                    fetched_at,
                    article.simhash as i64,
                ],
                row_to_article,
            )
            .optional()?;
        let Some(inserted) = inserted else {
            return Ok(None);
        };
        for (band, value) in simhash_bands(inserted.simhash).into_iter().enumerate() {
            tx.execute(
                "INSERT OR REPLACE INTO article_bands (article_id, band, value, workspace_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![inserted.id, band as i64, value as i64, workspace_id],
            )?;
        }
        tx.commit()?;
        Ok(Some(inserted))
    }

    pub async fn get_article(&self, id: &str) -> Result<Option<Article>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_ARTICLE} FROM articles WHERE id = ?1");
        Ok(conn
            .query_row(&sql, params![id], row_to_article)
            .optional()?)
    }

    /// The feed, filtered. Every clause is optional and additive; the ordering is
    /// always newest-published-first, because the *ranked* ordering is computed by
    /// the ranker over this set and must not be half-applied by SQL.
    pub async fn list_articles(
        &self,
        workspace_id: &str,
        query: &ArticleQuery,
    ) -> Result<Vec<Article>> {
        let mut sql = format!("SELECT {COLS_ARTICLE} FROM articles WHERE workspace_id = ?1");
        let mut binds: Vec<SqlValue> = vec![SqlValue::Text(workspace_id.to_string())];
        if let Some(source_id) = &query.source_id {
            binds.push(SqlValue::Text(source_id.clone()));
            sql.push_str(&format!(" AND source_id = ?{}", binds.len()));
        }
        if let Some(story_id) = &query.story_id {
            binds.push(SqlValue::Text(story_id.clone()));
            sql.push_str(&format!(" AND story_id = ?{}", binds.len()));
        }
        if let Some(since) = query.since {
            binds.push(SqlValue::Integer(since));
            sql.push_str(&format!(" AND published_at >= ?{}", binds.len()));
        }
        match query.unread {
            Some(true) => sql.push_str(" AND read_at IS NULL"),
            Some(false) => sql.push_str(" AND read_at IS NOT NULL"),
            None => {}
        }
        match query.saved {
            Some(true) => sql.push_str(" AND saved_at IS NOT NULL"),
            Some(false) => sql.push_str(" AND saved_at IS NULL"),
            None => {}
        }
        // Archived rows are excluded unless asked for. Archiving that left the item
        // in the feed would not be archiving.
        match query.archived {
            Some(true) => sql.push_str(" AND archived_at IS NOT NULL"),
            Some(false) | None => sql.push_str(" AND archived_at IS NULL"),
        }
        if !query.include_duplicates {
            sql.push_str(" AND duplicate_of IS NULL");
        }
        binds.push(SqlValue::Integer(clamp_limit(query.limit) as i64));
        sql.push_str(&format!(
            " ORDER BY published_at DESC, id ASC LIMIT ?{}",
            binds.len()
        ));

        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), row_to_article)?;
        Ok(collect_readable(rows)?)
    }

    /// Set or clear one of the three reader marks. The column name comes from a
    /// closed enum, so there is no path from caller input into the SQL.
    pub async fn set_article_mark(
        &self,
        id: &str,
        mark: ArticleMark,
        on: bool,
        at: i64,
    ) -> Result<bool> {
        let column = mark.column();
        let conn = self.conn.lock().await;
        let n = conn.execute(
            &format!("UPDATE articles SET {column} = ?2 WHERE id = ?1"),
            params![id, if on { Some(at) } else { None }],
        )?;
        Ok(n > 0)
    }

    /// Attach an article to a cluster. Does NOT recount the story — the clusterer
    /// assigns a batch and then calls [`NewsStore::recount_story`] once, because
    /// recounting per article is a `COUNT(DISTINCT …)` per article.
    pub async fn assign_story(&self, article_id: &str, story_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE articles SET story_id = ?2 WHERE id = ?1",
            params![article_id, story_id],
        )?;
        Ok(n > 0)
    }

    /// Mark an article as a near-duplicate of an earlier one. Pass `None` to clear.
    pub async fn set_duplicate_of(
        &self,
        article_id: &str,
        original_id: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE articles SET duplicate_of = ?2 WHERE id = ?1",
            params![article_id, original_id],
        )?;
        Ok(n > 0)
    }

    /// Candidate near-duplicates for a hash: every article sharing at least one of
    /// its four bands, with its own hash so the caller can compute the exact Hamming
    /// distance.
    ///
    /// This is a RECALL step, not a decision — the band probe is deliberately loose
    /// and the exact distance check is what rejects the false positives. Returned
    /// oldest-first so "the earliest copy is the original" is a stable rule rather
    /// than whatever order the index happened to yield.
    pub async fn simhash_candidates(
        &self,
        workspace_id: &str,
        hash: u64,
        since: i64,
    ) -> Result<Vec<(String, u64)>> {
        let bands = simhash_bands(hash);
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT a.id, a.simhash FROM article_bands b
               JOIN articles a ON a.id = b.article_id
              WHERE b.workspace_id = ?1
                AND a.fetched_at >= ?2
                AND ((b.band = 0 AND b.value = ?3)
                  OR (b.band = 1 AND b.value = ?4)
                  OR (b.band = 2 AND b.value = ?5)
                  OR (b.band = 3 AND b.value = ?6))
              GROUP BY a.id
              ORDER BY a.published_at ASC, a.id ASC
              LIMIT {MAX_LIMIT}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                workspace_id,
                since,
                bands[0] as i64,
                bands[1] as i64,
                bands[2] as i64,
                bands[3] as i64,
            ],
            |row| {
                let id: String = row.get(0)?;
                let stored: i64 = row.get(1)?;
                Ok((id, stored as u64))
            },
        )?;
        Ok(collect_readable(rows)?)
    }

    /// Articles the clusterer has not placed yet, oldest first — the clustering pass
    /// is order-dependent (each article joins the best cluster as it stands at that
    /// moment), so replaying it must feed them in the order they arrived.
    pub async fn unclustered_articles(
        &self,
        workspace_id: &str,
        limit: usize,
    ) -> Result<Vec<Article>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_ARTICLE} FROM articles
              WHERE workspace_id = ?1 AND story_id IS NULL
              ORDER BY published_at ASC, id ASC
              LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![workspace_id, clamp_limit(Some(limit)) as i64],
            row_to_article,
        )?;
        Ok(collect_readable(rows)?)
    }

    /// Retention: drop articles published before `before`, keeping anything the
    /// reader explicitly saved. Returns how many went.
    ///
    /// Ordered cascade again, and saved articles are the exception that makes it
    /// worth reading: read-later is a promise, so retention must not quietly break
    /// it.
    pub async fn prune_articles(&self, workspace_id: &str, before: i64) -> Result<usize> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let doomed = "SELECT id FROM articles
                       WHERE workspace_id = ?1 AND published_at < ?2 AND saved_at IS NULL";
        let touched_stories: Vec<String> = tx
            .prepare(&format!(
                "SELECT DISTINCT story_id FROM articles
                  WHERE story_id IS NOT NULL AND id IN ({doomed})"
            ))?
            .query_map(params![workspace_id, before], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        tx.execute(
            &format!("DELETE FROM topic_matches WHERE article_id IN ({doomed})"),
            params![workspace_id, before],
        )?;
        tx.execute(
            &format!("DELETE FROM article_bands WHERE article_id IN ({doomed})"),
            params![workspace_id, before],
        )?;
        let n = tx.execute(
            "DELETE FROM articles
              WHERE workspace_id = ?1 AND published_at < ?2 AND saved_at IS NULL",
            params![workspace_id, before],
        )?;
        for story_id in &touched_stories {
            recount_story_in(&tx, story_id)?;
            drop_story_if_empty(&tx, story_id)?;
        }
        tx.commit()?;
        Ok(n)
    }
}
