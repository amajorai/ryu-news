use super::*;

// ── Workspaces ─────────────────────────────────────────────────────────────────

impl NewsStore {
    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let conn = self.conn.lock().await;
        let sql =
            format!("SELECT {COLS_WORKSPACE} FROM workspaces ORDER BY created_at ASC, name ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_workspace)?;
        Ok(collect_readable(rows)?)
    }

    pub async fn get_workspace(&self, id: &str) -> Result<Option<Workspace>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_WORKSPACE} FROM workspaces WHERE id = ?1");
        Ok(conn
            .query_row(&sql, params![id], row_to_workspace)
            .optional()?)
    }

    /// Create the workspace if it is not already there, and return it either way.
    ///
    /// Every route defaults `workspace_id` to `"default"` rather than requiring it,
    /// so a caller naming a workspace that does not exist yet is a normal first
    /// write and not an error.
    pub async fn ensure_workspace(&self, id: &str, name: &str) -> Result<Workspace> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR IGNORE INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![id, name.trim(), now_ms()],
        )?;
        let sql = format!("SELECT {COLS_WORKSPACE} FROM workspaces WHERE id = ?1");
        Ok(conn.query_row(&sql, params![id], row_to_workspace)?)
    }
}

// ── Settings ───────────────────────────────────────────────────────────────────

impl NewsStore {
    /// The workspace's engine settings, or the defaults when it has never saved any.
    ///
    /// Falls back to the defaults on a corrupt blob too, rather than failing: a
    /// settings row that will not parse must not stop the poll loop, because the
    /// only way to fix it is through a UI that needs the app running.
    pub async fn get_settings(&self, workspace_id: &str) -> Result<NewsSettings> {
        let conn = self.conn.lock().await;
        let raw: Option<String> = conn
            .query_row(
                "SELECT json FROM settings WHERE workspace_id = ?1",
                params![workspace_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw
            .and_then(|json| match serde_json::from_str(&json) {
                Ok(settings) => Some(settings),
                Err(e) => {
                    tracing::warn!(workspace_id, error = %e, "unreadable news settings; using defaults");
                    None
                }
            })
            .unwrap_or_default())
    }

    pub async fn save_settings(&self, workspace_id: &str, settings: &NewsSettings) -> Result<()> {
        let json = serde_json::to_string(settings)?;
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO settings (workspace_id, json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(workspace_id) DO UPDATE SET json = ?2, updated_at = ?3",
            params![workspace_id, json, now_ms()],
        )?;
        Ok(())
    }
}
