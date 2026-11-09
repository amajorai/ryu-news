use super::*;

// ── Briefs ─────────────────────────────────────────────────────────────────────

impl NewsStore {
    pub async fn create_brief(
        &self,
        workspace_id: &str,
        trigger: BriefTrigger,
        items: &[BriefItem],
        generated_at: i64,
    ) -> Result<Brief> {
        let items_json = serde_json::to_string(items)?;
        let article_count: i64 = items.iter().map(|i| i.sources.len() as i64).sum();
        let conn = self.conn.lock().await;
        let sql = format!(
            "INSERT INTO briefs
               (id, workspace_id, generated_at, trigger_kind, story_count, article_count, items)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ryu_seal('briefs.items', ?1, ?7))
             RETURNING {COLS_BRIEF}"
        );
        Ok(conn.query_row(
            &sql,
            params![
                new_id(ID_BRIEF),
                workspace_id,
                generated_at,
                trigger.as_str(),
                items.len() as i64,
                article_count,
                items_json,
            ],
            row_to_brief,
        )?)
    }

    pub async fn get_brief(&self, id: &str) -> Result<Option<Brief>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_BRIEF} FROM briefs WHERE id = ?1");
        Ok(conn.query_row(&sql, params![id], row_to_brief).optional()?)
    }

    pub async fn list_briefs(&self, workspace_id: &str, limit: usize) -> Result<Vec<Brief>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_BRIEF} FROM briefs WHERE workspace_id = ?1
             ORDER BY generated_at DESC, id ASC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![workspace_id, clamp_limit(Some(limit)) as i64],
            row_to_brief,
        )?;
        Ok(collect_readable(rows)?)
    }

    /// The newest brief, which is what the `brief` MCP tool answers with when no id
    /// is given.
    pub async fn latest_brief(&self, workspace_id: &str) -> Result<Option<Brief>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_BRIEF} FROM briefs WHERE workspace_id = ?1
             ORDER BY generated_at DESC, id ASC LIMIT 1"
        );
        Ok(conn
            .query_row(&sql, params![workspace_id], row_to_brief)
            .optional()?)
    }
}
