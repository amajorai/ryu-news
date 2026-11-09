use super::*;

// ── Purges (the manifest's two `data_categories`) ──────────────────────────────

impl NewsStore {
    /// `news_articles` — "Every article, story cluster, dedupe fingerprint, brief and
    /// read/saved/archived mark on this node will be deleted. The sources you
    /// subscribed to and the watches you wrote are kept."
    ///
    /// The order is the cascade: match rows and band rows are reached through the
    /// articles, so they go first. Topic bursts go too — a burst is a statement
    /// about articles that no longer exist, and keeping it would leave an audit trail
    /// pointing at nothing.
    pub async fn purge_articles(&self, workspace_id: &str) -> Result<PurgeCounts> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM topic_matches WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        tx.execute(
            "DELETE FROM topic_bursts WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        tx.execute(
            "DELETE FROM article_bands WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        let articles = tx.execute(
            "DELETE FROM articles WHERE workspace_id = ?1",
            params![workspace_id],
        )? as i64;
        let stories = tx.execute(
            "DELETE FROM stories WHERE workspace_id = ?1",
            params![workspace_id],
        )? as i64;
        let briefs = tx.execute(
            "DELETE FROM briefs WHERE workspace_id = ?1",
            params![workspace_id],
        )? as i64;
        tx.commit()?;
        Ok(PurgeCounts {
            articles,
            stories,
            briefs,
            ..PurgeCounts::default()
        })
    }

    /// `news_sources` — "Every source subscription and every saved topic query on
    /// this node will be deleted, and polling stops. Articles already collected are
    /// kept but nothing new arrives."
    ///
    /// So the articles stay and their `source_id` is left dangling, on purpose: this
    /// schema has no foreign keys (module docs), an article carries the text a reader
    /// wants without its subscription row, and the alternative — deleting the archive
    /// because someone unsubscribed — is the opposite of what the copy promises.
    pub async fn purge_sources_and_topics(&self, workspace_id: &str) -> Result<PurgeCounts> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM topic_matches WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        tx.execute(
            "DELETE FROM topic_bursts WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        let topics = tx.execute(
            "DELETE FROM topics WHERE workspace_id = ?1",
            params![workspace_id],
        )? as i64;
        let sources = tx.execute(
            "DELETE FROM sources WHERE workspace_id = ?1",
            params![workspace_id],
        )? as i64;
        tx.commit()?;
        Ok(PurgeCounts {
            sources,
            topics,
            ..PurgeCounts::default()
        })
    }
}

// ── Health ─────────────────────────────────────────────────────────────────────

impl NewsStore {
    /// Counts for `/health`. Reading them is the point: a probe that only proves the
    /// process is alive answers 200 while every request 500s on an unreadable file.
    pub async fn health_counts(&self) -> Result<HealthCounts> {
        let conn = self.conn.lock().await;
        let count = |table: &str| -> rusqlite::Result<i64> {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        };
        Ok(HealthCounts {
            sources: count("sources")?,
            articles: count("articles")?,
            stories: count("stories")?,
            topics: count("topics")?,
            briefs: count("briefs")?,
        })
    }
}
