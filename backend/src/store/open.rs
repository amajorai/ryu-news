use super::*;

impl NewsStore {
    /// Open (creating if needed) the DB at `path` and migrate it. The path is
    /// injected by the caller (`paths::data_dir().join("news.db")`) so this module has
    /// no opinion about where the node's data lives.
    ///
    /// The sealing key is resolved with the DB's directory as the plugin data
    /// dir (see [`crate::seal_keys`]).
    pub fn open(path: PathBuf) -> Result<Self> {
        let dir = path
            .parent()
            .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf);
        let keys = PluginKeys::resolve(PLUGIN_ID, &dir)?;
        Self::open_with_keys(path, &keys)
    }

    /// [`Self::open`] with explicit keys (the stdio MCP process passes keys
    /// that never generate a standalone root).
    pub fn open_with_keys(path: PathBuf, keys: &PluginKeys) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("creating parent dir for news.db")?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening news db at {}", path.display()))?;
        Self::from_connection(conn, keys, Some(&path))
    }

    pub(super) fn from_connection(
        mut conn: Connection,
        keys: &PluginKeys,
        path: Option<&std::path::Path>,
    ) -> Result<Self> {
        // Before any statement that could rewrite a cell.
        ryu_crypto::content_sqlite::enable_secure_delete(&conn)?;
        Self::prepare(&conn)?;
        let seal = open_store(&mut conn, keys, STORE, &column_plans()?, path)
            .context("sealing news.db")?;
        let blind = match seal.current() {
            StoreSeal::Sealing(_) => match (
                keys.blind_index(STORE, "sources.feed_url"),
                keys.blind_index(STORE, "topics.name"),
            ) {
                (Some(feed), Some(name)) => Some(Arc::new((feed, name))),
                _ => None,
            },
            _ => None,
        };
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
            seal,
            blind,
        };
        let filled = {
            let conn = store.conn.try_lock().expect("exclusive at open");
            store.backfill_tokens(&conn)?
        };
        if filled > 0 && store.blind.is_some() {
            if let Some(path) = path {
                let conn = store.conn.try_lock().expect("exclusive at open");
                if let Err(error) = finish_store(&conn, path, STORE) {
                    tracing::warn!("news.db: scrubbing freed pages failed: {error:#}");
                }
            }
        }
        Ok(store)
    }

    /// The sealing health item: `{state, detail?, unreadable_rows}`.
    pub fn seal_health(&self) -> serde_json::Value {
        self.seal.health()
    }

    /// Whether this process cannot use the sealed content: it holds no key (or
    /// another key) for a sealed store, including one the sidecar sealed after
    /// this keyless process opened it.
    pub async fn locked_out(&self) -> bool {
        let conn = self.conn.lock().await;
        if let Err(error) = self.seal.refresh_compat(&conn) {
            tracing::warn!("news: could not re-check the store's sealing: {error:#}");
        }
        self.seal.current().ensure_writable().is_err()
    }

    /// The uniqueness token of a feed URL (compat mode: the URL itself).
    pub(super) fn feed_token(&self, feed_url: &str) -> String {
        match &self.blind {
            Some(blind) => blind.0.token(feed_url),
            None => feed_url.to_owned(),
        }
    }

    /// The uniqueness token of a watch name (compat mode: the name itself).
    pub(super) fn name_token(&self, name: &str) -> String {
        match &self.blind {
            Some(blind) => blind.1.token(name),
            None => name.to_owned(),
        }
    }

    /// Fill `feed_url_bi` / `name_bi` for rows without a current token: rows
    /// from before v2 (NULL) and, once a key is held, rows a compat-mode
    /// process wrote (which hold the value itself). Returns the rows updated.
    pub(super) fn backfill_tokens(&self, conn: &Connection) -> Result<u64> {
        if matches!(self.seal.current(), StoreSeal::Unhealthy(_)) {
            return Ok(0);
        }
        let mut updated = 0u64;
        let mut folded = 0u64;
        for (table, column, token_column) in [
            ("sources", "feed_url", "feed_url_bi"),
            ("topics", "name", "name_bi"),
        ] {
            let pending = if self.blind.is_some() {
                format!("{token_column} IS NULL OR substr({token_column}, 1, 4) <> 'bi1:'")
            } else {
                format!("{token_column} IS NULL")
            };
            // Oldest first, so the subscription that keeps the token on a
            // collision is the one the user made first.
            let rows: Vec<(String, String)> = {
                let mut stmt = conn.prepare(&format!(
                    "SELECT id, {column} FROM {table} WHERE {pending} ORDER BY created_at, id"
                ))?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for (id, stored) in rows {
                let Ok(value) = self
                    .seal
                    .open_text(&stored, &format!("{table}.{column}"), &id)
                else {
                    continue;
                };
                let token = if column == "feed_url" {
                    self.feed_token(&value)
                } else {
                    self.name_token(&value)
                };
                match conn.execute(
                    &format!(
                        "UPDATE {table} SET {token_column} = ?2 WHERE id = ?1 AND {column} IS ?3"
                    ),
                    params![id, token, stored],
                ) {
                    Ok(n) => updated += n as u64,
                    Err(error) if is_unique_violation(&error) => {
                        // A duplicate of a row that already holds this token: an
                        // older build wrote it while the plaintext unique index was
                        // gone (downgrade), or a keyless compat write raced the
                        // sidecar's sealing. Fold it into the survivor through the
                        // same ordered cascade an unsubscribe uses, so the open
                        // never fails on it.
                        let tx = conn.unchecked_transaction()?;
                        if table == "sources" {
                            delete_source_in(&tx, &id)?;
                        } else {
                            delete_topic_in(&tx, &id)?;
                        }
                        tx.commit()?;
                        folded += 1;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        if folded > 0 {
            tracing::warn!(
                "news.db: folded {folded} duplicate subscription or watch row(s) left by an older build"
            );
        }
        Ok(updated + folded)
    }

    /// In-memory store. A plain `pub fn`, not `#[cfg(test)]`, so the modules that
    /// land beside this one (the clusterer, the burst test, the ranker, the MCP
    /// server) can build a REAL store in their own tests instead of a mock that
    /// agrees with them by construction.
    ///
    /// It seals under a random key, so every test exercises the sealed path.
    pub fn open_in_memory() -> Result<Self> {
        use base64::Engine as _;
        use rand::RngCore as _;
        let mut root = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut root);
        let keys = PluginKeys::sealing(
            ryu_crypto::PluginCipher::from_subkey_b64(
                PLUGIN_ID,
                &base64::engine::general_purpose::STANDARD.encode(root),
            )?,
            crate::seal_keys::KeySource::Delivered,
        );
        Self::from_connection(Connection::open_in_memory()?, &keys, None)
    }

    /// Pragmas then migrations. Both paths call this so an in-memory store is
    /// byte-for-byte the same schema as a real one — a divergence here would make
    /// every module test a lie.
    pub(super) fn prepare(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            // WAL: readers never block the single writer, which matters because the
            // poll loop writes while the companion polls the feed.
            // synchronous=NORMAL: safe under WAL (a crash can lose the last commit,
            // not corrupt the file) and avoids an fsync per article.
            // busy_timeout: this process serializes its own writes behind the mutex,
            // but another process holding the file (a shell, a backup) would
            // otherwise fail instantly instead of waiting.
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )
        .context("applying news db pragmas")?;
        Self::migrate(conn)
    }

    /// The `PRAGMA user_version` ladder.
    ///
    /// Every arm must be safe to re-run, because this runs on EVERY open: an arm
    /// that can fail does not fail once, it refuses to boot the sidecar forever —
    /// on exactly the databases the fix was meant to repair. That is why every
    /// statement in [`V1_DDL`] is `IF NOT EXISTS` or `INSERT OR IGNORE`, and why any
    /// future arm that adds a UNIQUE index must DELETE the offending rows first.
    pub(super) fn migrate(conn: &Connection) -> Result<()> {
        let current: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current >= SCHEMA_VERSION {
            return Ok(());
        }
        if current < 1 {
            conn.execute_batch(V1_DDL)
                .context("applying news schema v1")?;
        }
        if current < 2 {
            for (table, column) in [("sources", "feed_url_bi"), ("topics", "name_bi")] {
                if let Err(error) =
                    conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} TEXT"), [])
                {
                    if !error.to_string().contains("duplicate column name") {
                        return Err(error).with_context(|| format!("adding {table}.{column}"));
                    }
                }
            }
            conn.execute_batch(V2_DDL)
                .context("applying news schema v2")?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .context("stamping news schema version")?;
        Ok(())
    }
}
