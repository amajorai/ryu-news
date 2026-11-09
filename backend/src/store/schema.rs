/// The complete v1 schema.
///
/// Collapsed into ONE statement batch rather than replayed as a migration history,
/// because there are no existing databases to migrate — this app has never shipped.
/// Every table is declared in its final shape.
pub(super) const V1_DDL: &str = "
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
