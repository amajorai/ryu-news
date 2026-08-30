//! SQLite persistence for the whole Wire spine (`~/.ryu/news.db`).
//!
//! Every SQL statement in this app lives here, one method per operation. The
//! ingest, clustering, burst and ranking modules are pure functions over the types
//! this module hands them — which is what makes the claim "the parts you would be
//! angry to see wrong are computed" testable, because nothing in the spine can
//! reach a database.
//!
//! ## No foreign keys — a deliberate choice, not an omission
//!
//! `PRAGMA foreign_keys` is **per-connection and not persisted in the file**, so a
//! schema with real `ON DELETE CASCADE` behaves differently depending on which code
//! path opened the connection — silent orphans on one, cascades on the other, and
//! invisible until the data is already gone. Deletes here are explicit ordered
//! cascades inside one transaction ([`NewsStore::delete_source`],
//! [`NewsStore::purge_articles`], [`NewsStore::purge_sources_and_topics`]), which is
//! auditable and connection-independent.
//!
//! One reference is consequently allowed to dangle on purpose:
//! `purge_sources_and_topics` removes subscriptions while KEEPING the articles they
//! produced, because the manifest's data-category says so in as many words. An
//! article whose `source_id` no longer resolves renders with its stored source name
//! and still clusters.
//!
//! ## Uniqueness is an index, never a pre-insert SELECT
//!
//! Three real UNIQUE indexes carry the deduplication this app is built on:
//! `sources(workspace_id, feed_url)` so importing the same OPML twice is a no-op,
//! `articles(workspace_id, canonical_url)` so the same page reached through six
//! tracking links lands once, and `articles(source_id, guid)` so a site that
//! rewrites its URLs does not re-import its history. Check-then-insert is racy under
//! concurrent handlers plus a poll loop; `INSERT … ON CONFLICT DO NOTHING RETURNING`
//! is not, and it tells the caller which one happened in a single round trip.
//!
//! The SimHash layer is a different mechanism and cannot be an index: near-duplicate
//! is a distance, not an equality. It gets [`ARTICLE_BAND_COUNT`] rows per article in
//! `article_bands` so a lookup is a hash probe per band rather than a scan of every
//! article ever collected.
//!
//! ## Locking
//!
//! One `Arc<tokio::sync::Mutex<Connection>>` (the async mutex, matching `ryu-social`
//! / `ryu-teams`) — a single writer with WAL underneath. `busy_timeout` still
//! matters because WAL admits readers from OTHER processes (a `sqlite3` shell, a
//! backup), about which this process's mutex knows nothing.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use tokio::sync::Mutex;

use crate::models::*;

/// The schema version this build expects. Bump it and add a `v<N>` arm in
/// [`NewsStore::migrate`] when the shape changes.
///
/// A `PRAGMA user_version` ladder rather than bare `CREATE TABLE IF NOT EXISTS`:
/// `IF NOT EXISTS` cannot add a COLUMN to a table that already exists, so the moment
/// a later change needs one it would have to retrofit the whole versioning scheme
/// onto live user databases. Paying for it now costs one integer.
const SCHEMA_VERSION: i32 = 1;

/// How many bands a 64-bit SimHash is split into for indexed lookup.
///
/// Four bands of 16 bits is the standard split for a Hamming threshold of 3: by the
/// pigeonhole principle two hashes within distance 3 must agree exactly on at least
/// one of four bands, so probing all four bands finds every true near-duplicate
/// (plus some false positives the exact distance check then rejects). Raising the
/// threshold past 3 — which `DedupeAggressiveness::Aggressive` does — makes the
/// probe lossy rather than exact; that is a recall trade the aggressive setting
/// accepts, not a bug, and it is why the ALGORITHMS default stays at 3.
pub const ARTICLE_BAND_COUNT: usize = 4;

/// Split a SimHash into its four 16-bit bands.
///
/// This lives with the schema rather than with the SimHash computation because the
/// band split IS the index layout: change it and `article_bands` has to be rebuilt,
/// which is a migration, not a tuning knob.
pub fn simhash_bands(hash: u64) -> [u16; ARTICLE_BAND_COUNT] {
    [
        (hash & 0xFFFF) as u16,
        ((hash >> 16) & 0xFFFF) as u16,
        ((hash >> 32) & 0xFFFF) as u16,
        ((hash >> 48) & 0xFFFF) as u16,
    ]
}

/// SQLite-backed store for the whole app. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct NewsStore {
    conn: Arc<Mutex<Connection>>,
}

impl NewsStore {
    /// Open (creating if needed) the DB at `path` and migrate it. The path is
    /// injected by the caller (`paths::ryu_dir().join("news.db")`) so this module has
    /// no opinion about where the node's data lives.
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("creating parent dir for news.db")?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening news db at {}", path.display()))?;
        Self::prepare(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// In-memory store. A plain `pub fn`, not `#[cfg(test)]`, so the modules that
    /// land beside this one (the clusterer, the burst test, the ranker, the MCP
    /// server) can build a REAL store in their own tests instead of a mock that
    /// agrees with them by construction.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::prepare(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Pragmas then migrations. Both paths call this so an in-memory store is
    /// byte-for-byte the same schema as a real one — a divergence here would make
    /// every module test a lie.
    fn prepare(conn: &Connection) -> Result<()> {
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
    fn migrate(conn: &Connection) -> Result<()> {
        let current: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current >= SCHEMA_VERSION {
            return Ok(());
        }
        if current < 1 {
            conn.execute_batch(V1_DDL)
                .context("applying news schema v1")?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .context("stamping news schema version")?;
        Ok(())
    }
}

/// The complete v1 schema.
///
/// Collapsed into ONE statement batch rather than replayed as a migration history,
/// because there are no existing databases to migrate — this app has never shipped.
/// Every table is declared in its final shape.
const V1_DDL: &str = "
CREATE TABLE IF NOT EXISTS workspaces (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sources (
  id                   TEXT PRIMARY KEY,
  workspace_id         TEXT NOT NULL,
  title                TEXT NOT NULL,
  feed_url             TEXT NOT NULL,
  site_url             TEXT,
  kind                 TEXT NOT NULL,
  enabled              INTEGER NOT NULL DEFAULT 1,
  etag                 TEXT,
  last_modified        TEXT,
  last_fetch_at        INTEGER,
  last_success_at      INTEGER,
  consecutive_failures INTEGER NOT NULL DEFAULT 0,
  next_fetch_at        INTEGER,
  last_error           TEXT,
  created_at           INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sources_workspace
  ON sources(workspace_id, title);
-- Subscribing twice to one feed is the normal outcome of importing an OPML file
-- that overlaps the existing list, so it must be a no-op rather than a duplicate.
CREATE UNIQUE INDEX IF NOT EXISTS idx_sources_feed_url
  ON sources(workspace_id, feed_url);
-- The poll sweep's hot predicate: `enabled = 1 AND next_fetch_at <= now`. The
-- workspace-leading index above cannot serve it, so without this every sweep is a
-- full table scan.
CREATE INDEX IF NOT EXISTS idx_sources_due
  ON sources(enabled, next_fetch_at);

CREATE TABLE IF NOT EXISTS articles (
  id            TEXT PRIMARY KEY,
  workspace_id  TEXT NOT NULL,
  source_id     TEXT NOT NULL,
  story_id      TEXT,
  guid          TEXT,
  url           TEXT NOT NULL,
  canonical_url TEXT NOT NULL,
  title         TEXT NOT NULL,
  author        TEXT,
  summary       TEXT,
  content       TEXT,
  published_at  INTEGER NOT NULL,
  fetched_at    INTEGER NOT NULL,
  -- The 64-bit SimHash, bit-reinterpreted into SQLite's signed INTEGER. See
  -- `row_to_article` for why this is a cast and not a conversion.
  simhash       INTEGER NOT NULL,
  duplicate_of  TEXT,
  read_at       INTEGER,
  saved_at      INTEGER,
  archived_at   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_articles_feed
  ON articles(workspace_id, published_at DESC);
CREATE INDEX IF NOT EXISTS idx_articles_story
  ON articles(story_id, published_at DESC);
CREATE INDEX IF NOT EXISTS idx_articles_source
  ON articles(source_id, published_at DESC);
-- Dedupe layer 1: the same page reached through six tracking links is one row.
CREATE UNIQUE INDEX IF NOT EXISTS idx_articles_canonical
  ON articles(workspace_id, canonical_url);
-- A feed's own item id, so a site that rewrites its URLs does not re-import its
-- whole history. NULLs are distinct in a SQLite unique index, which is exactly what
-- is wanted here: a feed with no guid at all is governed by the canonical URL alone.
CREATE UNIQUE INDEX IF NOT EXISTS idx_articles_guid
  ON articles(source_id, guid);

-- Dedupe layer 2: the banded SimHash index. A separate table rather than four
-- columns on `articles` because the probe is `band = ? AND value = ?` across all
-- four bands at once — one index serves every band here, where columns would need
-- four.
CREATE TABLE IF NOT EXISTS article_bands (
  article_id   TEXT NOT NULL,
  band         INTEGER NOT NULL,
  value        INTEGER NOT NULL,
  workspace_id TEXT NOT NULL,
  PRIMARY KEY (article_id, band)
);
CREATE INDEX IF NOT EXISTS idx_article_bands_probe
  ON article_bands(workspace_id, band, value);

CREATE TABLE IF NOT EXISTS stories (
  id                     TEXT PRIMARY KEY,
  workspace_id           TEXT NOT NULL,
  title                  TEXT,
  summary                TEXT,
  -- JSON arrays. Opaque to SQL on purpose: they are set operands the clusterer
  -- reads whole, never something a query filters on.
  centroid_shingles      TEXT NOT NULL DEFAULT '[]',
  centroid_member_count  INTEGER NOT NULL DEFAULT 0,
  entities               TEXT NOT NULL DEFAULT '[]',
  article_count          INTEGER NOT NULL DEFAULT 0,
  source_count           INTEGER NOT NULL DEFAULT 0,
  notified_source_count  INTEGER NOT NULL DEFAULT 0,
  followed               INTEGER NOT NULL DEFAULT 0,
  first_seen_at          INTEGER NOT NULL,
  last_seen_at           INTEGER NOT NULL
);
-- The clustering candidate scan: clusters whose `last_seen_at` is inside the
-- window, in a STABLE order so an article lands in the same cluster on a replay.
CREATE INDEX IF NOT EXISTS idx_stories_recent
  ON stories(workspace_id, last_seen_at DESC);
CREATE INDEX IF NOT EXISTS idx_stories_followed
  ON stories(workspace_id, followed);

CREATE TABLE IF NOT EXISTS topics (
  id           TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  name         TEXT NOT NULL,
  query        TEXT NOT NULL,
  ast          TEXT NOT NULL,
  enabled      INTEGER NOT NULL DEFAULT 1,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_topics_name
  ON topics(workspace_id, name);

CREATE TABLE IF NOT EXISTS topic_matches (
  topic_id     TEXT NOT NULL,
  article_id   TEXT NOT NULL,
  workspace_id TEXT NOT NULL,
  matched_at   INTEGER NOT NULL,
  PRIMARY KEY (topic_id, article_id)
);
-- The burst baseline reads seven days of one topic's match timestamps.
CREATE INDEX IF NOT EXISTS idx_topic_matches_window
  ON topic_matches(topic_id, matched_at);

CREATE TABLE IF NOT EXISTS topic_bursts (
  id             TEXT PRIMARY KEY,
  workspace_id   TEXT NOT NULL,
  topic_id       TEXT NOT NULL,
  z_score        REAL NOT NULL,
  -- Named `match_count` rather than `count`: `count` reads as the aggregate
  -- function at every call site and the confusion is not worth the two characters.
  match_count    INTEGER NOT NULL,
  baseline_mean  REAL NOT NULL,
  baseline_stdev REAL NOT NULL,
  hour_of_day    INTEGER NOT NULL,
  article_ids    TEXT NOT NULL DEFAULT '[]',
  detected_at    INTEGER NOT NULL
);
-- Serves both readers: the audit trail for one topic, and the cooldown check, which
-- is the most recent row rather than a `last_burst_at` column duplicating it.
CREATE INDEX IF NOT EXISTS idx_topic_bursts_topic
  ON topic_bursts(topic_id, detected_at DESC);

CREATE TABLE IF NOT EXISTS briefs (
  id            TEXT PRIMARY KEY,
  workspace_id  TEXT NOT NULL,
  generated_at  INTEGER NOT NULL,
  -- `trigger` is a SQLite keyword (CREATE TRIGGER); the column carries the suffix
  -- while the wire field stays `trigger`.
  trigger_kind  TEXT NOT NULL,
  story_count   INTEGER NOT NULL DEFAULT 0,
  article_count INTEGER NOT NULL DEFAULT 0,
  items         TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS idx_briefs_workspace
  ON briefs(workspace_id, generated_at DESC);

CREATE TABLE IF NOT EXISTS settings (
  workspace_id TEXT PRIMARY KEY,
  json         TEXT NOT NULL,
  updated_at   INTEGER NOT NULL
);

-- Seeded so a first-run client always has somewhere to write and `?workspace_id=`
-- can be defaulted instead of required on every route.
INSERT OR IGNORE INTO workspaces (id, name, created_at)
  VALUES ('default', 'Default', 0);
";

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
fn json_list(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn row_to_workspace(row: &Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: row.get(0)?,
        name: row.get(1)?,
        created_at: row.get(2)?,
    })
}

fn row_to_source(row: &Row<'_>) -> rusqlite::Result<Source> {
    let kind: String = row.get(5)?;
    Ok(Source {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        title: row.get(2)?,
        feed_url: row.get(3)?,
        site_url: row.get(4)?,
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
        last_error: row.get(13)?,
        created_at: row.get(14)?,
    })
}

fn row_to_article(row: &Row<'_>) -> rusqlite::Result<Article> {
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

fn row_to_story(row: &Row<'_>) -> rusqlite::Result<Story> {
    let centroid: String = row.get(4)?;
    let entities: String = row.get(6)?;
    Ok(Story {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        title: row.get(2)?,
        summary: row.get(3)?,
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

fn row_to_topic(row: &Row<'_>) -> rusqlite::Result<Topic> {
    let ast: String = row.get(4)?;
    Ok(Topic {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        name: row.get(2)?,
        query: row.get(3)?,
        // A corrupt AST becomes `null`, which the matcher reads as "matches
        // nothing". The stored query text is still there for the user to re-save,
        // which is the only thing that can actually repair it.
        ast: serde_json::from_str(&ast).unwrap_or(serde_json::Value::Null),
        enabled: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn row_to_burst(row: &Row<'_>) -> rusqlite::Result<TopicBurst> {
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

fn row_to_brief(row: &Row<'_>) -> rusqlite::Result<Brief> {
    let trigger: String = row.get(3)?;
    let items: String = row.get(6)?;
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
const COLS_WORKSPACE: &str = "id, name, created_at";
const COLS_SOURCE: &str = "id, workspace_id, title, feed_url, site_url, kind, enabled, etag, \
                           last_modified, last_fetch_at, last_success_at, consecutive_failures, \
                           next_fetch_at, last_error, created_at";
const COLS_ARTICLE: &str = "id, workspace_id, source_id, story_id, guid, url, canonical_url, \
                            title, author, summary, content, published_at, fetched_at, simhash, \
                            duplicate_of, read_at, saved_at, archived_at";
const COLS_STORY: &str = "id, workspace_id, title, summary, centroid_shingles, \
                          centroid_member_count, entities, article_count, source_count, \
                          notified_source_count, followed, first_seen_at, last_seen_at";
const COLS_TOPIC: &str = "id, workspace_id, name, query, ast, enabled, created_at, updated_at";
const COLS_BURST: &str = "id, workspace_id, topic_id, z_score, match_count, baseline_mean, \
                          baseline_stdev, hour_of_day, article_ids, detected_at";
const COLS_BRIEF: &str =
    "id, workspace_id, generated_at, trigger_kind, story_count, article_count, items";

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

// ── Workspaces ─────────────────────────────────────────────────────────────────

impl NewsStore {
    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let conn = self.conn.lock().await;
        let sql =
            format!("SELECT {COLS_WORKSPACE} FROM workspaces ORDER BY created_at ASC, name ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_workspace)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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

// ── Sources ────────────────────────────────────────────────────────────────────

impl NewsStore {
    pub async fn list_sources(&self, workspace_id: &str) -> Result<Vec<Source>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_SOURCE} FROM sources WHERE workspace_id = ?1
             ORDER BY title ASC, id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![workspace_id], row_to_source)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
                created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, NULL, NULL, NULL, NULL, 0, ?7, NULL, ?7)
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
            "UPDATE sources SET title = ?2, site_url = ?3, enabled = ?4 WHERE id = ?1",
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
        let n = tx.execute("DELETE FROM sources WHERE id = ?1", params![id])?;
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
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
                        last_error = ?3
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
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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

// ── Stories ────────────────────────────────────────────────────────────────────

/// Recompute a story's denormalized counts from its members.
///
/// Shared by every path that changes membership so there is exactly ONE writer for
/// `article_count` / `source_count` / `last_seen_at`. `notified_source_count` is
/// deliberately untouched: it records what `story.developing` last announced, and
/// resetting it here would re-announce growth that was already reported.
fn recount_story_in(conn: &Connection, story_id: &str) -> rusqlite::Result<usize> {
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
fn drop_story_if_empty(conn: &Connection, story_id: &str) -> rusqlite::Result<usize> {
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
             VALUES (?1, ?2, NULL, NULL, ?3, 1, ?4, 0, 0, 0, 0, ?5, ?5)",
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
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
                SET centroid_shingles = ?2, entities = ?3, centroid_member_count = ?4
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
                SET title = COALESCE(?2, title), summary = COALESCE(?3, summary)
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
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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

// ── Topics ─────────────────────────────────────────────────────────────────────

impl NewsStore {
    pub async fn list_topics(&self, workspace_id: &str) -> Result<Vec<Topic>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_TOPIC} FROM topics WHERE workspace_id = ?1
             ORDER BY name ASC, id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![workspace_id], row_to_topic)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
            "INSERT INTO topics (id, workspace_id, name, query, ast, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)
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
            "UPDATE topics SET name = ?2, query = ?3, ast = ?4, enabled = ?5, updated_at = ?6
              WHERE id = ?1",
            params![id, name.trim(), query.trim(), ast_json, enabled, now_ms()],
        )?;
        Ok(n > 0)
    }

    /// Delete a watch, its match history and its burst record. Ordered cascade: the
    /// children go first, because they are reached only through the topic.
    pub async fn delete_topic(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM topic_matches WHERE topic_id = ?1", params![id])?;
        tx.execute("DELETE FROM topic_bursts WHERE topic_id = ?1", params![id])?;
        let n = tx.execute("DELETE FROM topics WHERE id = ?1", params![id])?;
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
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
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
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> NewsStore {
        NewsStore::open_in_memory().expect("in-memory store")
    }

    async fn feed(store: &NewsStore) -> Source {
        store
            .create_source(
                DEFAULT_WORKSPACE_ID,
                &NewSource {
                    title: "Reuters".into(),
                    feed_url: "https://example.test/feed.xml".into(),
                    site_url: None,
                    kind: SourceKind::Rss,
                },
            )
            .await
            .unwrap()
            .expect("a fresh feed url subscribes")
    }

    /// Insert an article and return it, panicking on the duplicate case — the tests
    /// that care about duplicates check `insert_article` directly.
    async fn insert(
        store: &NewsStore,
        source_id: &str,
        canonical: &str,
        hash: u64,
        at: i64,
    ) -> Article {
        store
            .insert_article(
                DEFAULT_WORKSPACE_ID,
                &article(source_id, canonical, hash, at),
                at,
            )
            .await
            .unwrap()
            .expect("a fresh canonical url inserts")
    }

    fn article(source_id: &str, canonical: &str, simhash: u64, published_at: i64) -> NewArticle {
        NewArticle {
            source_id: source_id.to_string(),
            guid: None,
            url: format!("{canonical}?utm_source=x"),
            canonical_url: canonical.to_string(),
            title: "Regulator opens inquiry into the merger".into(),
            author: None,
            summary: None,
            content: None,
            published_at,
            simhash,
        }
    }

    /// The one test that actually EXECUTES the DDL. `cargo check` cannot: a typo in
    /// `V1_DDL` is a string literal that compiles perfectly and then panics on the
    /// first real `open()`. Run this before anything else in this crate.
    #[tokio::test]
    async fn migrations_apply_on_a_fresh_db_and_seed_the_default_workspace() {
        let store = store();
        let workspaces = store.list_workspaces().await.unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].id, DEFAULT_WORKSPACE_ID);
        assert_eq!(workspaces[0].name, DEFAULT_WORKSPACE_NAME);
        // Every table is reachable, which is the part `cargo check` cannot tell you.
        let counts = store.health_counts().await.unwrap();
        assert_eq!(counts.articles, 0);
        assert_eq!(counts.briefs, 0);
    }

    /// `migrate()` runs on EVERY open, so an arm that can fail does not fail once —
    /// it refuses to boot the sidecar forever, on exactly the databases the fix was
    /// meant to repair. This replays v1 over a database that already holds data (by
    /// resetting `user_version`, which is what an interrupted migration leaves
    /// behind) and asserts both that it succeeds and that it destroys nothing.
    #[tokio::test]
    async fn migrating_again_over_a_populated_db_is_a_no_op() {
        let conn = Connection::open_in_memory().unwrap();
        NewsStore::prepare(&conn).unwrap();
        conn.execute(
            "INSERT INTO sources
               (id, workspace_id, title, feed_url, site_url, kind, enabled, consecutive_failures,
                created_at)
             VALUES ('src_1', 'default', 'Reuters', 'https://example.test/f', NULL, 'rss', 1, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO articles
               (id, workspace_id, source_id, url, canonical_url, title, published_at, fetched_at,
                simhash)
             VALUES ('ar_1', 'default', 'src_1', 'https://example.test/a?utm_source=x',
                     'https://example.test/a', 'Headline', 0, 0, 7)",
            [],
        )
        .unwrap();

        conn.pragma_update(None, "user_version", 0).unwrap();
        NewsStore::migrate(&conn).expect("re-running the v1 arm must not fail");

        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let sources: i64 = conn
            .query_row("SELECT COUNT(*) FROM sources", [], |r| r.get(0))
            .unwrap();
        let articles: i64 = conn
            .query_row("SELECT COUNT(*) FROM articles", [], |r| r.get(0))
            .unwrap();
        assert_eq!((sources, articles), (1, 1));
        // The seed is `INSERT OR IGNORE`, so replaying it does not duplicate the
        // default workspace either.
        let workspaces: i64 = conn
            .query_row("SELECT COUNT(*) FROM workspaces", [], |r| r.get(0))
            .unwrap();
        assert_eq!(workspaces, 1);
    }

    #[tokio::test]
    async fn subscribing_twice_to_one_feed_is_a_no_op_not_a_duplicate() {
        let store = store();
        let first = feed(&store).await;
        let again = store
            .create_source(
                DEFAULT_WORKSPACE_ID,
                &NewSource {
                    title: "Reuters (again)".into(),
                    feed_url: "https://example.test/feed.xml".into(),
                    site_url: None,
                    kind: SourceKind::Rss,
                },
            )
            .await
            .unwrap();
        assert!(
            again.is_none(),
            "the UNIQUE index must swallow the re-import"
        );
        assert_eq!(
            store
                .list_sources(DEFAULT_WORKSPACE_ID)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(first.title, "Reuters");
    }

    #[tokio::test]
    async fn the_same_canonical_url_lands_once_however_it_was_reached() {
        let store = store();
        let source = feed(&store).await;
        let inserted = store
            .insert_article(
                DEFAULT_WORKSPACE_ID,
                &article(&source.id, "https://example.test/a", 0xDEAD_BEEF, 1_000),
                1_000,
            )
            .await
            .unwrap();
        assert!(inserted.is_some());
        let duplicate = store
            .insert_article(
                DEFAULT_WORKSPACE_ID,
                &article(&source.id, "https://example.test/a", 0xDEAD_BEEF, 2_000),
                2_000,
            )
            .await
            .unwrap();
        assert!(duplicate.is_none(), "the conflict must report, not raise");
    }

    /// The half of the hashes with the high bit set are the ones a `try_into()`
    /// would lose, so this pins the reinterpret round trip specifically.
    #[tokio::test]
    async fn a_simhash_with_the_high_bit_set_survives_the_round_trip() {
        let store = store();
        let source = feed(&store).await;
        let hash = 0xFFFF_FFFF_FFFF_FFF0_u64;
        let inserted = store
            .insert_article(
                DEFAULT_WORKSPACE_ID,
                &article(&source.id, "https://example.test/high", hash, 1_000),
                1_000,
            )
            .await
            .unwrap()
            .unwrap();
        let read_back = store.get_article(&inserted.id).await.unwrap().unwrap();
        assert_eq!(read_back.simhash, hash);

        // And it is findable through its own bands, which is what makes near-dupe
        // detection a probe rather than a scan.
        let candidates = store
            .simhash_candidates(DEFAULT_WORKSPACE_ID, hash, 0)
            .await
            .unwrap();
        assert_eq!(candidates, vec![(inserted.id, hash)]);
    }

    #[tokio::test]
    async fn band_probe_finds_a_near_duplicate_and_ignores_an_unrelated_hash() {
        let store = store();
        let source = feed(&store).await;
        let original = 0x0123_4567_89AB_CDEF_u64;
        // One bit away: three of the four bands still match exactly.
        let near = original ^ 1;
        let unrelated = 0xFFFF_0000_FFFF_0000_u64;
        for (canonical, hash) in [
            ("https://example.test/1", original),
            ("https://example.test/2", unrelated),
        ] {
            store
                .insert_article(
                    DEFAULT_WORKSPACE_ID,
                    &article(&source.id, canonical, hash, 1_000),
                    1_000,
                )
                .await
                .unwrap()
                .unwrap();
        }
        let candidates = store
            .simhash_candidates(DEFAULT_WORKSPACE_ID, near, 0)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].1, original);
    }

    #[tokio::test]
    async fn a_failed_fetch_backs_the_source_off_and_surfaces_it_as_failing() {
        let store = store();
        let source = feed(&store).await;
        for _ in 0..FAILING_AFTER_CONSECUTIVE_FAILURES {
            assert!(store
                .record_source_failure(&source.id, 10_000, "connection refused")
                .await
                .unwrap());
        }
        let after = store.get_source(&source.id).await.unwrap().unwrap();
        assert_eq!(
            after.consecutive_failures,
            FAILING_AFTER_CONSECUTIVE_FAILURES
        );
        assert_eq!(after.health(), SourceHealth::Failing);
        assert_eq!(
            after.next_fetch_at,
            Some(10_000 + backoff_hours(FAILING_AFTER_CONSECUTIVE_FAILURES) * 3_600_000)
        );
        // …and it is out of the due set until that backoff elapses.
        assert!(store.due_sources(10_001, 10).await.unwrap().is_empty());

        store
            .record_source_success(&source.id, 20_000, Some("W/\"abc\""), None, 20_500)
            .await
            .unwrap();
        let healed = store.get_source(&source.id).await.unwrap().unwrap();
        assert_eq!(healed.consecutive_failures, 0);
        assert_eq!(healed.health(), SourceHealth::Healthy);
        assert_eq!(healed.etag.as_deref(), Some("W/\"abc\""));
        assert!(healed.last_error.is_none());
    }

    #[tokio::test]
    async fn a_new_cluster_is_born_already_announced_and_only_a_new_outlet_is_growth() {
        let store = store();
        let source = feed(&store).await;
        let first = insert(&store, &source.id, "https://example.test/1", 1, 4_000).await;
        let story = store
            .create_story(
                DEFAULT_WORKSPACE_ID,
                &first.id,
                &["a b c".into()],
                &["Regulator".into()],
                5_000,
            )
            .await
            .unwrap();
        store.set_story_followed(&story.id, true).await.unwrap();

        // A cluster that just opened has nothing "developing" about it. The seed at
        // creation is the only thing standing between this and paging on every new
        // single-outlet report.
        assert_eq!(story.source_count, 1);
        assert_eq!(story.notified_source_count, story.source_count);
        assert!(store
            .followed_stories_that_grew(DEFAULT_WORKSPACE_ID)
            .await
            .unwrap()
            .is_empty());

        // A second article from the SAME outlet is not growth — `source_count` is
        // distinct sources, which is the number the manifest's payload reports.
        let same_outlet = insert(&store, &source.id, "https://example.test/2", 2, 4_500).await;
        store
            .assign_story(&same_outlet.id, &story.id)
            .await
            .unwrap();
        let recounted = store.recount_story(&story.id).await.unwrap().unwrap();
        assert_eq!((recounted.article_count, recounted.source_count), (2, 1));
        assert!(store
            .followed_stories_that_grew(DEFAULT_WORKSPACE_ID)
            .await
            .unwrap()
            .is_empty());

        // A second OUTLET is.
        let other_outlet = store
            .create_source(
                DEFAULT_WORKSPACE_ID,
                &NewSource {
                    title: "AP".into(),
                    feed_url: "https://ap.test/feed.xml".into(),
                    site_url: None,
                    kind: SourceKind::Rss,
                },
            )
            .await
            .unwrap()
            .unwrap();
        let joined = insert(&store, &other_outlet.id, "https://ap.test/1", 3, 5_500).await;
        store.assign_story(&joined.id, &story.id).await.unwrap();
        store.recount_story(&story.id).await.unwrap();
        let grew = store
            .followed_stories_that_grew(DEFAULT_WORKSPACE_ID)
            .await
            .unwrap();
        assert_eq!(grew.len(), 1);
        assert_eq!(grew[0].source_count, 2);

        // Announcing it clears it, and the CAS ignores a stale re-announce.
        assert!(store.mark_story_notified(&story.id, 2).await.unwrap());
        assert!(!store.mark_story_notified(&story.id, 1).await.unwrap());
        assert!(store
            .followed_stories_that_grew(DEFAULT_WORKSPACE_ID)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn deleting_a_source_takes_its_articles_and_empties_its_stories() {
        let store = store();
        let source = feed(&store).await;
        let item = insert(&store, &source.id, "https://example.test/1", 9, 1_000).await;
        let story = store
            .create_story(DEFAULT_WORKSPACE_ID, &item.id, &[], &[], 1_000)
            .await
            .unwrap();

        assert!(store.delete_source(&source.id).await.unwrap());
        assert!(store.get_source(&source.id).await.unwrap().is_none());
        assert!(store.get_article(&item.id).await.unwrap().is_none());
        // The story had exactly one member, so it is not a story any more.
        assert!(store.get_story(&story.id).await.unwrap().is_none());
        let counts = store.health_counts().await.unwrap();
        assert_eq!((counts.articles, counts.stories), (0, 0));
        // Deleting something that is already gone reports it rather than pretending.
        assert!(!store.delete_source(&source.id).await.unwrap());
    }

    #[tokio::test]
    async fn the_two_data_category_purges_delete_exactly_what_they_promise() {
        let store = store();
        let source = feed(&store).await;
        let topic = store
            .create_topic(
                DEFAULT_WORKSPACE_ID,
                "Semiconductor export controls",
                "semiconductor AND export",
                &serde_json::json!({"kind": "term", "value": "semiconductor"}),
            )
            .await
            .unwrap()
            .unwrap();
        let item = store
            .insert_article(
                DEFAULT_WORKSPACE_ID,
                &article(&source.id, "https://example.test/1", 11, 1_000),
                1_000,
            )
            .await
            .unwrap()
            .unwrap();
        store
            .record_topic_match(DEFAULT_WORKSPACE_ID, &topic.id, &item.id, 1_000)
            .await
            .unwrap();
        store
            .create_brief(DEFAULT_WORKSPACE_ID, BriefTrigger::Manual, &[], 1_000)
            .await
            .unwrap();

        // `news_articles`: the collection goes, the subscriptions and watches stay.
        let purged = store.purge_articles(DEFAULT_WORKSPACE_ID).await.unwrap();
        assert_eq!((purged.articles, purged.briefs), (1, 1));
        let counts = store.health_counts().await.unwrap();
        assert_eq!(counts.articles, 0);
        assert_eq!(counts.sources, 1);
        assert_eq!(counts.topics, 1);
        // The fingerprints went with them; a leftover band row would resurrect a
        // deleted article as a dedupe candidate forever.
        assert!(store
            .simhash_candidates(DEFAULT_WORKSPACE_ID, 11, 0)
            .await
            .unwrap()
            .is_empty());

        // `news_sources`: the subscriptions and watches go, whatever is left of the
        // archive stays.
        store
            .insert_article(
                DEFAULT_WORKSPACE_ID,
                &article(&source.id, "https://example.test/kept", 12, 2_000),
                2_000,
            )
            .await
            .unwrap()
            .unwrap();
        let purged = store
            .purge_sources_and_topics(DEFAULT_WORKSPACE_ID)
            .await
            .unwrap();
        assert_eq!((purged.sources, purged.topics), (1, 1));
        let counts = store.health_counts().await.unwrap();
        assert_eq!((counts.sources, counts.topics, counts.articles), (0, 0, 1));
    }

    #[tokio::test]
    async fn article_filters_narrow_and_archived_rows_leave_the_feed() {
        let store = store();
        let source = feed(&store).await;
        let mut ids = Vec::new();
        for (n, canonical) in ["https://a.test/1", "https://a.test/2", "https://a.test/3"]
            .into_iter()
            .enumerate()
        {
            ids.push(
                store
                    .insert_article(
                        DEFAULT_WORKSPACE_ID,
                        &article(&source.id, canonical, n as u64, 1_000 + n as i64),
                        1_000,
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
            );
        }
        store
            .set_article_mark(&ids[0], ArticleMark::Read, true, 2_000)
            .await
            .unwrap();
        store
            .set_article_mark(&ids[1], ArticleMark::Archived, true, 2_000)
            .await
            .unwrap();
        store
            .set_article_mark(&ids[2], ArticleMark::Saved, true, 2_000)
            .await
            .unwrap();

        let feed_view = store
            .list_articles(DEFAULT_WORKSPACE_ID, &ArticleQuery::default())
            .await
            .unwrap();
        assert_eq!(feed_view.len(), 2, "the archived one is out of the feed");
        assert!(feed_view.iter().all(|a| !a.is_archived()));
        // Newest published first, and stable on ties.
        assert!(feed_view[0].published_at >= feed_view[1].published_at);

        let unread = store
            .list_articles(
                DEFAULT_WORKSPACE_ID,
                &ArticleQuery {
                    unread: Some(true),
                    ..ArticleQuery::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, ids[2]);

        let saved = store
            .list_articles(
                DEFAULT_WORKSPACE_ID,
                &ArticleQuery {
                    saved: Some(true),
                    ..ArticleQuery::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(saved.len(), 1);
        assert!(saved[0].is_saved());

        // Clearing a mark puts the row back.
        store
            .set_article_mark(&ids[1], ArticleMark::Archived, false, 3_000)
            .await
            .unwrap();
        assert_eq!(
            store
                .list_articles(DEFAULT_WORKSPACE_ID, &ArticleQuery::default())
                .await
                .unwrap()
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn retention_prunes_old_articles_but_never_a_saved_one() {
        let store = store();
        let source = feed(&store).await;
        let old = store
            .insert_article(
                DEFAULT_WORKSPACE_ID,
                &article(&source.id, "https://a.test/old", 1, 1_000),
                1_000,
            )
            .await
            .unwrap()
            .unwrap();
        let kept = store
            .insert_article(
                DEFAULT_WORKSPACE_ID,
                &article(&source.id, "https://a.test/saved", 2, 1_000),
                1_000,
            )
            .await
            .unwrap()
            .unwrap();
        store
            .set_article_mark(&kept.id, ArticleMark::Saved, true, 1_500)
            .await
            .unwrap();

        assert_eq!(
            store
                .prune_articles(DEFAULT_WORKSPACE_ID, 5_000)
                .await
                .unwrap(),
            1
        );
        assert!(store.get_article(&old.id).await.unwrap().is_none());
        assert!(store.get_article(&kept.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_watch_is_unique_by_name_and_its_matches_feed_the_baseline() {
        let store = store();
        let source = feed(&store).await;
        let ast = serde_json::json!({"kind": "term", "value": "merger"});
        let topic = store
            .create_topic(DEFAULT_WORKSPACE_ID, "Mergers", "merger", &ast)
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .create_topic(DEFAULT_WORKSPACE_ID, "Mergers", "other", &ast)
            .await
            .unwrap()
            .is_none());

        let item = store
            .insert_article(
                DEFAULT_WORKSPACE_ID,
                &article(&source.id, "https://a.test/1", 4, 1_000),
                1_000,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .record_topic_match(DEFAULT_WORKSPACE_ID, &topic.id, &item.id, 1_000)
            .await
            .unwrap());
        // Re-running the matcher over an article it already matched must not
        // double-count it into the burst baseline.
        assert!(!store
            .record_topic_match(DEFAULT_WORKSPACE_ID, &topic.id, &item.id, 1_100)
            .await
            .unwrap());
        assert_eq!(
            store.topic_match_times(&topic.id, 0, 10_000).await.unwrap(),
            vec![1_000]
        );
        assert_eq!(
            store
                .topic_match_articles(&topic.id, 0, 10_000, 10)
                .await
                .unwrap()
                .len(),
            1
        );

        // The cooldown is the latest burst row, not a column.
        assert!(store.last_burst(&topic.id).await.unwrap().is_none());
        let burst = store
            .record_burst(&TopicBurst {
                id: String::new(),
                workspace_id: DEFAULT_WORKSPACE_ID.into(),
                topic_id: topic.id.clone(),
                z_score: 4.7,
                count: 9,
                baseline_mean: 1.2,
                baseline_stdev: 1.6,
                hour_of_day: 14,
                article_ids: vec![item.id.clone()],
                detected_at: 9_000,
            })
            .await
            .unwrap();
        assert!(burst.id.starts_with(ID_BURST));
        let latest = store.last_burst(&topic.id).await.unwrap().unwrap();
        assert_eq!(latest.article_ids, vec![item.id]);
        assert!((latest.z_score - 4.7).abs() < 1e-9);

        assert!(store.delete_topic(&topic.id).await.unwrap());
        assert!(store.last_burst(&topic.id).await.unwrap().is_none());
        assert!(store
            .topic_match_times(&topic.id, 0, 10_000)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn settings_default_until_saved_and_survive_a_round_trip() {
        let store = store();
        assert_eq!(
            store.get_settings(DEFAULT_WORKSPACE_ID).await.unwrap(),
            NewsSettings::default()
        );
        let settings = NewsSettings {
            brief_time: Some("07:30".into()),
            brief_timezone: Some("Europe/London".into()),
            dedupe: DedupeAggressiveness::Aggressive,
            item_cap: 40,
            ..NewsSettings::default()
        };
        store
            .save_settings(DEFAULT_WORKSPACE_ID, &settings)
            .await
            .unwrap();
        assert_eq!(
            store.get_settings(DEFAULT_WORKSPACE_ID).await.unwrap(),
            settings
        );
    }

    #[tokio::test]
    async fn a_corrupt_settings_blob_degrades_to_defaults_instead_of_stopping_the_poll_loop() {
        let store = store();
        {
            let conn = store.conn.lock().await;
            conn.execute(
                "INSERT INTO settings (workspace_id, json, updated_at) VALUES (?1, '{oh no', 0)",
                params![DEFAULT_WORKSPACE_ID],
            )
            .unwrap();
        }
        assert_eq!(
            store.get_settings(DEFAULT_WORKSPACE_ID).await.unwrap(),
            NewsSettings::default()
        );
    }

    #[tokio::test]
    async fn briefs_list_newest_first() {
        let store = store();
        let items = vec![BriefItem {
            story_id: "st_1".into(),
            title: "Regulator opens inquiry".into(),
            summary: "Two sentences.".into(),
            sources: vec![BriefSource {
                article_id: "ar_1".into(),
                source: "Reuters".into(),
                url: "https://a.test/1".into(),
            }],
        }];
        store
            .create_brief(DEFAULT_WORKSPACE_ID, BriefTrigger::Scheduled, &items, 1_000)
            .await
            .unwrap();
        let newest = store
            .create_brief(DEFAULT_WORKSPACE_ID, BriefTrigger::Manual, &items, 2_000)
            .await
            .unwrap();
        assert_eq!(newest.story_count, 1);
        assert_eq!(newest.article_count, 1);
        assert_eq!(newest.trigger, BriefTrigger::Manual);

        let listed = store.list_briefs(DEFAULT_WORKSPACE_ID, 10).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, newest.id);
        assert_eq!(
            store
                .latest_brief(DEFAULT_WORKSPACE_ID)
                .await
                .unwrap()
                .unwrap()
                .id,
            newest.id
        );
        assert_eq!(listed[0].items[0].sources[0].source, "Reuters");
    }

    #[test]
    fn bands_partition_the_hash_without_losing_a_bit() {
        let hash = 0x0123_4567_89AB_CDEF_u64;
        let bands = simhash_bands(hash);
        assert_eq!(bands, [0xCDEF, 0x89AB, 0x4567, 0x0123]);
        let rebuilt = bands
            .iter()
            .enumerate()
            .fold(0_u64, |acc, (i, b)| acc | ((*b as u64) << (16 * i)));
        assert_eq!(rebuilt, hash);
    }
}
