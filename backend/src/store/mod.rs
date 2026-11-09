//! SQLite persistence for the whole Wire spine (`<data dir>/plugin-data/@ryu+news/news.db`).
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
//! ## Sealed at rest
//!
//! What the user chose and wrote is sealed per value (`encb:v1:`, bound to
//! `<table>.<column>` and the row id) under this app's plugin subkey (see
//! [`crate::seal_sql`]): source titles, feed and site URLs (a private feed URL
//! can carry a token) and fetch errors; watch names, queries and parsed
//! queries; story titles, summaries and entity lists; and brief items. The
//! feed-URL and watch-name uniqueness moved to keyed blind tokens
//! (`sources.feed_url_bi`, `topics.name_bi`, `bi1:` + HMAC), so subscribing twice
//! is still a no-op decided by a UNIQUE index; the source list and the watch
//! list sort by title and name after opening. Articles are copies of public
//! feed items and stay plaintext: their canonical URL and guid carry the
//! deduplication indexes and the matcher and ranker read their text.
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
use ryu_crypto::content_sqlite::{finish_store, ColumnPlan, KeyKind};
use ryu_crypto::BlindIndex;
use tokio::sync::Mutex;

use crate::models::*;
use crate::seal_keys::{PluginKeys, StoreSeal};
use crate::seal_sql::{collect_readable, open_store, opened, opened_opt, SqlSeal};

/// The plugin id (the sealing key's scope).
pub const PLUGIN_ID: &str = "@ryu/news";
/// The registered content-store name (part of every key derivation; never rename).
pub const STORE: &str = "news.db";

/// The sealed columns, as `(table, column)`.
const SEALED: [(&str, &str); 11] = [
    ("sources", "title"),
    ("sources", "feed_url"),
    ("sources", "site_url"),
    ("sources", "last_error"),
    ("topics", "name"),
    ("topics", "query"),
    ("topics", "ast"),
    ("stories", "title"),
    ("stories", "summary"),
    ("stories", "entities"),
    ("briefs", "items"),
];

/// The sealed columns (`encb:v1:`, bound to `<table>.<column>` and the row id).
pub fn column_plans() -> Result<Vec<ColumnPlan>> {
    SEALED
        .iter()
        .map(|(table, column)| ColumnPlan::text(table, column, "id", KeyKind::Text))
        .collect()
}

/// The v2 arm: keyed blind tokens take over the two uniqueness rules whose
/// columns are now sealed. Every statement is safe to re-run.
const V2_DDL: &str = "
DROP INDEX IF EXISTS idx_sources_feed_url;
DROP INDEX IF EXISTS idx_topics_name;
DROP INDEX IF EXISTS idx_sources_workspace;
CREATE INDEX IF NOT EXISTS idx_sources_workspace_id ON sources(workspace_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_sources_feed_url_bi ON sources(workspace_id, feed_url_bi);
CREATE UNIQUE INDEX IF NOT EXISTS idx_topics_name_bi ON topics(workspace_id, name_bi);
";

/// The schema version this build expects. Bump it and add a `v<N>` arm in
/// [`NewsStore::migrate`] when the shape changes.
///
/// A `PRAGMA user_version` ladder rather than bare `CREATE TABLE IF NOT EXISTS`:
/// `IF NOT EXISTS` cannot add a COLUMN to a table that already exists, so the moment
/// a later change needs one it would have to retrofit the whole versioning scheme
/// onto live user databases. Paying for it now costs one integer.
const SCHEMA_VERSION: i32 = 2;

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
    seal: SqlSeal,
    /// The blind indexes of `sources.feed_url` and `topics.name` (`None` when
    /// this process holds no key: compat mode stores the value itself).
    blind: Option<Arc<(BlindIndex, BlindIndex)>>,
}

mod articles;
mod briefs;
mod open;
mod purge;
mod rows;
mod schema;
mod sources;
mod stories;
mod topics;
mod workspaces;

#[cfg(test)]
mod tests;

pub use rows::PurgeCounts;

use rows::*;
use schema::V1_DDL;
use sources::delete_source_in;
use stories::{drop_story_if_empty, recount_story_in};
use topics::delete_topic_in;

/// Whether a statement failed on a UNIQUE index (`SQLITE_CONSTRAINT_UNIQUE`).
fn is_unique_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == rusqlite::ErrorCode::ConstraintViolation
                && e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    )
}
