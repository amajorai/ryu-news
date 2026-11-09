use super::*;

// ── Topics ─────────────────────────────────────────────────────────────────────

impl NewsStore {
    pub async fn list_topics(&self, workspace_id: &str) -> Result<Vec<Topic>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_TOPIC} FROM topics WHERE workspace_id = ?1");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![workspace_id], row_to_topic)?;
        // Names are sealed, so the order is applied after opening them.
        let mut topics = collect_readable(rows)?;
        topics.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        Ok(topics)
    }

    pub async fn get_topic(&self, id: &str) -> Result<Option<Topic>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_TOPIC} FROM topics WHERE id = ?1");
        Ok(conn.query_row(&sql, params![id], row_to_topic).optional()?)
    }

    /// Save a watch. Returns `None` when the workspace already has one by that name
    /// — again the UNIQUE index rather than a preceding SELECT.
    ///
    /// The AST is supplied already parsed: the parse must have SUCCEEDED before
    /// anything reaches the store, because a watch that saved with a broken query
    /// would silently match nothing, which is the failure this app refuses to have.
    pub async fn create_topic(
        &self,
        workspace_id: &str,
        name: &str,
        query: &str,
        ast: &serde_json::Value,
    ) -> Result<Option<Topic>> {
        let now = now_ms();
        let ast_json = serde_json::to_string(ast)?;
        let conn = self.conn.lock().await;
        let sql = format!(
            "INSERT INTO topics (id, workspace_id, name, query, ast, enabled, created_at, updated_at,
                                 name_bi)
             VALUES (?1, ?2, ryu_seal('topics.name', ?1, ?3), ryu_seal('topics.query', ?1, ?4),
                     ryu_seal('topics.ast', ?1, ?5), 1, ?6, ?6, ?7)
             ON CONFLICT DO NOTHING
             RETURNING {COLS_TOPIC}"
        );
        Ok(conn
            .query_row(
                &sql,
                params![
                    new_id(ID_TOPIC),
                    workspace_id,
                    name.trim(),
                    query.trim(),
                    ast_json,
                    now,
                    self.name_token(name.trim()),
                ],
                row_to_topic,
            )
            .optional()?)
    }

    pub async fn update_topic(
        &self,
        id: &str,
        name: &str,
        query: &str,
        ast: &serde_json::Value,
        enabled: bool,
    ) -> Result<bool> {
        let ast_json = serde_json::to_string(ast)?;
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE topics SET name = ryu_seal('topics.name', ?1, ?2),
                    query = ryu_seal('topics.query', ?1, ?3), ast = ryu_seal('topics.ast', ?1, ?4),
                    enabled = ?5, updated_at = ?6, name_bi = ?7
              WHERE id = ?1",
            params![
                id,
                name.trim(),
                query.trim(),
                ast_json,
                enabled,
                now_ms(),
                self.name_token(name.trim())
            ],
        )?;
        Ok(n > 0)
    }

    /// Delete a watch, its match history and its burst record. Ordered cascade: the
    /// children go first, because they are reached only through the topic.
    pub async fn delete_topic(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let n = delete_topic_in(&tx, id)?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Materialize the matcher's verdict for one article. Idempotent: re-running the
    /// matcher over an article it already matched must not double-count it into the
    /// burst baseline.
    pub async fn record_topic_match(
        &self,
        workspace_id: &str,
        topic_id: &str,
        article_id: &str,
        matched_at: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "INSERT OR IGNORE INTO topic_matches (topic_id, article_id, workspace_id, matched_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![topic_id, article_id, workspace_id, matched_at],
        )?;
        Ok(n > 0)
    }

    /// Raw match timestamps in a window, ascending.
    ///
    /// Raw rather than bucketed by SQL on purpose: the baseline is built per
    /// hour-of-day in the user's configured IANA zone, and `strftime('%H', …)` would
    /// silently bucket by UTC — which is off by a whole working day for half the
    /// world and shifts twice a year for most of the rest.
    pub async fn topic_match_times(&self, topic_id: &str, from: i64, to: i64) -> Result<Vec<i64>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT matched_at FROM topic_matches
              WHERE topic_id = ?1 AND matched_at >= ?2 AND matched_at < ?3
              ORDER BY matched_at ASC",
        )?;
        let rows = stmt.query_map(params![topic_id, from, to], |r| r.get(0))?;
        Ok(collect_readable(rows)?)
    }

    /// The articles that caused a window's matches — what a `topic.breaking` payload
    /// carries so the alert can be read rather than trusted.
    pub async fn topic_match_articles(
        &self,
        topic_id: &str,
        from: i64,
        to: i64,
        limit: usize,
    ) -> Result<Vec<Article>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {} FROM topic_matches m JOIN articles a ON a.id = m.article_id
              WHERE m.topic_id = ?1 AND m.matched_at >= ?2 AND m.matched_at < ?3
              ORDER BY m.matched_at DESC, a.id ASC
              LIMIT ?4",
            COLS_ARTICLE
                .split(", ")
                .map(|c| format!("a.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![topic_id, from, to, clamp_limit(Some(limit)) as i64],
            row_to_article,
        )?;
        Ok(collect_readable(rows)?)
    }

    /// The most recent burst for a topic. This IS the cooldown check — there is no
    /// `last_burst_at` column, because a second copy of the same fact is a second
    /// thing to keep in sync.
    pub async fn last_burst(&self, topic_id: &str) -> Result<Option<TopicBurst>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_BURST} FROM topic_bursts WHERE topic_id = ?1
             ORDER BY detected_at DESC LIMIT 1"
        );
        Ok(conn
            .query_row(&sql, params![topic_id], row_to_burst)
            .optional()?)
    }

    /// Record a burst that fired, with every number its payload carries.
    pub async fn record_burst(&self, burst: &TopicBurst) -> Result<TopicBurst> {
        let article_ids = serde_json::to_string(&burst.article_ids)?;
        let id = if burst.id.is_empty() {
            new_id(ID_BURST)
        } else {
            burst.id.clone()
        };
        let conn = self.conn.lock().await;
        let sql = format!(
            "INSERT INTO topic_bursts
               (id, workspace_id, topic_id, z_score, match_count, baseline_mean, baseline_stdev,
                hour_of_day, article_ids, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             RETURNING {COLS_BURST}"
        );
        Ok(conn.query_row(
            &sql,
            params![
                id,
                burst.workspace_id,
                burst.topic_id,
                burst.z_score,
                burst.count,
                burst.baseline_mean,
                burst.baseline_stdev,
                burst.hour_of_day,
                article_ids,
                burst.detected_at,
            ],
            row_to_burst,
        )?)
    }
}

/// The ordered watch cascade inside a caller's transaction (see
/// [`NewsStore::delete_topic`]). Returns the topics rows removed.
pub(super) fn delete_topic_in(tx: &rusqlite::Transaction<'_>, id: &str) -> rusqlite::Result<usize> {
    tx.execute("DELETE FROM topic_matches WHERE topic_id = ?1", params![id])?;
    tx.execute("DELETE FROM topic_bursts WHERE topic_id = ?1", params![id])?;
    tx.execute("DELETE FROM topics WHERE id = ?1", params![id])
}
