//! Domain types for the Wire spine — the wire contract the sidecar serves, the
//! store persists, and the companion UI renders from.
//!
//! Conventions, applied uniformly and deliberately:
//!
//! - **Ids are TEXT with a typed prefix** (`ws_`, `src_`, `ar_`, `st_`, `tp_`,
//!   `bst_`, `br_`) wrapping a UUIDv4. The prefixes are not decoration: they are the
//!   ones the manifest's `hook_events` payload examples already publish (`ar_3311`,
//!   `st_9f2c`, `tp_semis`, `br_2026-08-10`), and `articles.story_id` /
//!   `topic_matches.article_id` are cross-table references with no FK behind them
//!   (see [`crate::store`] for why), so a mis-wired id is otherwise invisible until
//!   it silently matches nothing.
//! - **Timestamps are `i64` epoch MILLIS**, never RFC-3339 strings — the hot
//!   predicates here are range scans (`published_at >= ?`, `last_seen_at >= ?`,
//!   `next_fetch_at <= ?`) and lexicographic string comparison is the wrong tool
//!   for them. The ONE exception is [`HeadlineSnapshot`], which is the KV contract a
//!   JavaScript sandbox reads; see that type.
//! - **Booleans are `bool` on the wire, `INTEGER` 0/1 in SQLite**, and the three
//!   reader marks are nullable *timestamps* rather than flags, because
//!   "read it 3 days ago" and "saved it just now" are both orderings the UI needs
//!   and a bare boolean throws that away.
//! - **Every field name is snake_case on the wire.**
//!
//! Enum columns carry no SQL `CHECK` constraint. The Rust enum plus its
//! [`std::str::FromStr`] IS the guard: a value that fails to parse degrades to a
//! documented default rather than failing a whole list query, which is what keeps
//! one corrupt row from blanking the feed.

use serde::{Deserialize, Serialize};

// ── Time + id helpers ──────────────────────────────────────────────────────────

/// Now, as epoch millis. Every timestamp this app writes is produced here so there
/// is exactly one clock read to stub in tests.
///
/// Nothing in the deterministic spine reads the clock itself — the clusterer, the
/// burst test and the ranker all take `now` as a parameter, so a replay produces
/// the same answer as the original run.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A fresh prefixed id. See the module docs for why the prefix exists.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

pub const ID_WORKSPACE: &str = "ws_";
pub const ID_SOURCE: &str = "src_";
pub const ID_ARTICLE: &str = "ar_";
pub const ID_STORY: &str = "st_";
pub const ID_TOPIC: &str = "tp_";
pub const ID_BURST: &str = "bst_";
pub const ID_BRIEF: &str = "br_";

/// Epoch millis as an RFC-3339 UTC string (`2026-08-10T09:12:00Z`).
///
/// Only the two surfaces that talk to something outside this process use it: the
/// KV snapshot the turn hook parses with `Date.parse`, and the event payloads. Every
/// stored column stays millis.
pub fn to_rfc3339(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The workspace every install starts with. Seeded `INSERT OR IGNORE` at migration
/// time so a first-run client always has somewhere to write, and so `?workspace_id=`
/// can be defaulted rather than required on every route.
pub const DEFAULT_WORKSPACE_ID: &str = "default";
pub const DEFAULT_WORKSPACE_NAME: &str = "Default";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

// ── Sources ────────────────────────────────────────────────────────────────────

/// How a source's items are obtained.
///
/// Stored rather than re-sniffed per poll: a server that answers
/// `application/xml` for an Atom document (many do) would otherwise flip the
/// classification between polls, and the parser it selects is what decides whether
/// the items are read at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// RSS 2.0 (and the RSS 0.9x/1.0 shapes the hand-written tokenizer folds into it).
    #[default]
    Rss,
    /// Atom 1.0.
    Atom,
    /// JSON Feed 1.1, which rides `serde_json` rather than the tokenizer.
    JsonFeed,
    /// No usable feed: the site is polled through the `web.extract` capability.
    /// Also the fallback for a feed that only carries truncated summaries.
    Extract,
}

impl SourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rss => "rss",
            Self::Atom => "atom",
            Self::JsonFeed => "json_feed",
            Self::Extract => "extract",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rss" => Some(Self::Rss),
            "atom" => Some(Self::Atom),
            "json_feed" | "json" => Some(Self::JsonFeed),
            "extract" => Some(Self::Extract),
            _ => None,
        }
    }
}

impl std::str::FromStr for SourceKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("unknown source kind \"{s}\""))
    }
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the UI shows next to a source. Derived from `consecutive_failures`, never
/// stored — a stored copy is a second fact about the same thing that can disagree
/// with the counter that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceHealth {
    Healthy,
    /// Missed at least once but not yet given up on.
    Degraded,
    /// Missed [`FAILING_AFTER_CONSECUTIVE_FAILURES`] times running. Surfaced loudly:
    /// a source that quietly stopped producing is indistinguishable from a quiet
    /// week, which is how a dead feed goes unnoticed for a month.
    Failing,
}

/// Consecutive misses before a source is surfaced as `failing`.
pub const FAILING_AFTER_CONSECUTIVE_FAILURES: i64 = 3;

/// Ceiling on the exponential backoff between retries of a failing source, in hours.
pub const MAX_BACKOFF_HOURS: i64 = 24;

/// How long to wait before retrying a source that has failed `consecutive_failures`
/// times in a row: `min(2^failures, 24)` hours.
///
/// Capped rather than uncapped because an uncapped doubling reaches "next Tuesday"
/// by the tenth failure, and a feed that came back up would not be noticed until
/// then. A pure function of the counter, so the retry schedule of a source can be
/// read off its row without knowing when the failures happened.
pub fn backoff_hours(consecutive_failures: i64) -> i64 {
    if consecutive_failures <= 0 {
        return 0;
    }
    // `1 << 63` overflows and `1 << 64` panics, so saturate the exponent well before
    // either — the cap makes everything past 5 the same answer anyway.
    let exponent = consecutive_failures.min(8) as u32;
    (1_i64 << exponent).min(MAX_BACKOFF_HOURS)
}

/// A feed subscription plus its health record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub workspace_id: String,
    pub title: String,
    /// The feed document's URL — the dedupe key for a subscription (a real UNIQUE
    /// index, so importing the same OPML twice is a no-op rather than a second copy
    /// of every article).
    pub feed_url: String,
    /// The human site the feed belongs to, for the UI's link-out. Optional because
    /// plenty of feeds do not declare one.
    pub site_url: Option<String>,
    pub kind: SourceKind,
    pub enabled: bool,
    /// Conditional-GET validators, replayed on the next fetch. Honouring these is
    /// the difference between a polite poller and one that re-downloads every feed
    /// on the node every 15 minutes.
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub last_fetch_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub consecutive_failures: i64,
    /// When the poll loop may next try this source. Written by the health update, so
    /// the backoff lives in the data rather than in a scheduler's memory — a
    /// restarted sidecar does not forget that a source is in a 16-hour backoff.
    pub next_fetch_at: Option<i64>,
    /// The last failure's message, shown in the UI. Not an error *chain*: it is
    /// rendered to a human, and this one is safe to show because it describes a
    /// remote the user chose.
    pub last_error: Option<String>,
    pub created_at: i64,
}

impl Source {
    pub fn health(&self) -> SourceHealth {
        if self.consecutive_failures >= FAILING_AFTER_CONSECUTIVE_FAILURES {
            SourceHealth::Failing
        } else if self.consecutive_failures > 0 {
            SourceHealth::Degraded
        } else {
            SourceHealth::Healthy
        }
    }
}

/// Everything needed to subscribe to a source. A struct rather than eight
/// positional arguments because four of them are `Option<String>` and a swapped
/// pair would compile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSource {
    pub title: String,
    pub feed_url: String,
    #[serde(default)]
    pub site_url: Option<String>,
    #[serde(default)]
    pub kind: SourceKind,
}

// ── Articles ───────────────────────────────────────────────────────────────────

/// One item from one source.
///
/// `canonical_url` and `simhash` are the two dedupe layers, and they catch different
/// things: the canonical URL collapses the same page reached through six tracking
/// links, the SimHash collapses one wire story rewritten under six headlines on six
/// sites. Neither subsumes the other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Article {
    pub id: String,
    pub workspace_id: String,
    pub source_id: String,
    /// The cluster this article was assigned to, or `None` until the clusterer has
    /// seen it.
    pub story_id: Option<String>,
    /// The feed's own item identifier (`<guid>` / Atom `<id>` / JSON Feed `id`),
    /// kept so a source that rewrites its URLs does not re-import its whole history.
    pub guid: Option<String>,
    /// The URL as published. THIS is what the user clicks — the canonical form is a
    /// dedupe key, not a link, and handing a user a stripped URL is how a paywall
    /// token or a required query param goes missing.
    pub url: String,
    pub canonical_url: String,
    pub title: String,
    pub author: Option<String>,
    pub summary: Option<String>,
    /// Full text where we have it: from the feed when it carries one, otherwise from
    /// the `web.extract` capability. `None` means shingling fell back to the title
    /// plus summary, which the clusterer needs to know.
    pub content: Option<String>,
    pub published_at: i64,
    pub fetched_at: i64,
    /// 64-bit SimHash over word 3-shingles. Stored as a SQLite INTEGER by
    /// reinterpreting the bits (`as i64`) — see [`crate::store`], because the
    /// round-trip is the kind of thing that works for half of all hashes.
    pub simhash: u64,
    /// Set when this article is a near-duplicate of an earlier one. The row is KEPT
    /// rather than dropped: the duplicate is evidence of how widely a story ran, and
    /// the story's source list would be a lie without it. The feed shows the
    /// canonical one.
    pub duplicate_of: Option<String>,
    pub read_at: Option<i64>,
    pub saved_at: Option<i64>,
    pub archived_at: Option<i64>,
}

impl Article {
    pub fn is_read(&self) -> bool {
        self.read_at.is_some()
    }
    pub fn is_saved(&self) -> bool {
        self.saved_at.is_some()
    }
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

/// The three reader marks, one per `/articles/:id/{read,save,archive}` route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArticleMark {
    Read,
    Saved,
    Archived,
}

impl ArticleMark {
    /// The column the mark writes. Returned as a `&'static str` from a closed match
    /// so it can be interpolated into SQL without becoming an injection seam — there
    /// is no path from caller input to this string other than the enum.
    pub const fn column(self) -> &'static str {
        match self {
            Self::Read => "read_at",
            Self::Saved => "saved_at",
            Self::Archived => "archived_at",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Saved => "saved",
            Self::Archived => "archived",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" => Some(Self::Read),
            "save" | "saved" => Some(Self::Saved),
            "archive" | "archived" => Some(Self::Archived),
            _ => None,
        }
    }
}

/// An article as ingest produces it, before the store assigns an id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewArticle {
    pub source_id: String,
    pub guid: Option<String>,
    pub url: String,
    pub canonical_url: String,
    pub title: String,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub content: Option<String>,
    pub published_at: i64,
    pub simhash: u64,
}

/// Which articles a list request wants. Every field is a narrowing filter; all-`None`
/// is "the whole feed, newest first".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArticleQuery {
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub story_id: Option<String>,
    /// `Some(true)` = only unread, `Some(false)` = only read.
    #[serde(default)]
    pub unread: Option<bool>,
    #[serde(default)]
    pub saved: Option<bool>,
    /// Archived articles are EXCLUDED unless this is `Some(true)`. The default
    /// matters: archiving that did not remove the item from the feed is not
    /// archiving.
    #[serde(default)]
    pub archived: Option<bool>,
    /// Only articles published at or after this instant.
    #[serde(default)]
    pub since: Option<i64>,
    /// Exclude rows marked as near-duplicates of another article. Defaults to
    /// excluding them; the story detail view is the one place that asks for them.
    #[serde(default)]
    pub include_duplicates: bool,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Default and ceiling for any list route, matching the sibling sidecars.
pub const DEFAULT_LIMIT: usize = 200;
pub const MAX_LIMIT: usize = 500;

/// Clamp a caller-supplied limit into `[1, MAX_LIMIT]`, defaulting when absent.
pub fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

// ── Stories (clusters) ─────────────────────────────────────────────────────────

/// A story: the same event as covered by *n* outlets.
///
/// This is the unit the app is built around — "eight outlets are covering this,
/// here is the spread" — so the counts are denormalized onto the row rather than
/// derived per read. They are recomputed by the store whenever membership changes
/// (see `NewsStore::recount_stories`), so there is exactly one writer for them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Story {
    pub id: String,
    pub workspace_id: String,
    /// The one neutral headline, written by a model over the members' eight
    /// different ones. `None` until titling runs — and the UI falls back to the
    /// earliest member's title, so a story is never nameless waiting on a model.
    pub title: Option<String>,
    /// Two sentences, written by a model at brief time. Same fallback rule.
    pub summary: Option<String>,
    /// The union of the first `centroid_k` members' shingles, frozen after that so a
    /// cluster cannot drift into a different story by slow accretion.
    pub centroid_shingles: Vec<String>,
    /// How many members contributed to the centroid. The freeze is DERIVED from
    /// this (`>= settings.centroid_k`) rather than stored as its own flag, so the
    /// two facts cannot disagree.
    pub centroid_member_count: i64,
    /// Deterministically extracted entities (capitalized n-grams minus a stopword
    /// list, plus quoted strings) — no model, so a replay clusters identically.
    pub entities: Vec<String>,
    pub article_count: i64,
    /// Distinct sources, which is the number the UI shows and the one
    /// `story.developing` compares against.
    pub source_count: i64,
    /// The `source_count` at the last `story.developing` emit.
    ///
    /// Seeded EQUAL to `source_count` when the cluster is created, not to zero: the
    /// manifest promises that a new cluster opening for the first time does not
    /// fire, and a zero seed makes the very next poll see growth that never
    /// happened.
    pub notified_source_count: i64,
    pub followed: bool,
    pub first_seen_at: i64,
    /// The newest member's arrival. The clustering candidate scan is bounded by it,
    /// and its index is what keeps that scan off a full table.
    pub last_seen_at: i64,
}

// ── Topics (watches) ───────────────────────────────────────────────────────────

/// A saved boolean query.
///
/// Both the source text and the parsed AST are stored: the text is what the user
/// edits, the AST is what the matcher evaluates, and keeping both means a query
/// never has to be re-parsed on the hot path. A parse error is rejected at SAVE time
/// with its column offset — a watch that silently matches nothing is worse than one
/// that refuses to save.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topic {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    /// The query as typed, e.g. `title:"export controls" AND (semiconductor OR chip)
    /// NOT opinion`.
    pub query: String,
    /// The parsed AST, serialized. `serde_json::Value` rather than the typed node
    /// enum so this module does not depend on the parser that lands beside it; the
    /// matcher deserializes it into its own type.
    pub ast: serde_json::Value,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One article that matched one topic. The join row is materialized rather than
/// re-evaluated on read because the burst baseline needs seven days of match
/// timestamps and re-running the matcher over a week of articles per poll is not a
/// thing to do every fifteen minutes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicMatch {
    pub topic_id: String,
    pub article_id: String,
    pub matched_at: i64,
}

/// A burst that fired, kept so the alert can be explained afterwards.
///
/// Every number the `topic.breaking` payload carries lives here, which is also what
/// makes this table the cooldown record: the latest row's `detected_at` is the
/// cooldown check, rather than a `last_burst_at` column that would be a second copy
/// of the same fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicBurst {
    pub id: String,
    pub workspace_id: String,
    pub topic_id: String,
    pub z_score: f64,
    pub count: i64,
    pub baseline_mean: f64,
    pub baseline_stdev: f64,
    /// Hour of day in the configured IANA zone, which is the bucket the baseline was
    /// built from. Stored because "0–23" means nothing without knowing whose day it
    /// was.
    pub hour_of_day: i64,
    pub article_ids: Vec<String>,
    pub detected_at: i64,
}

// ── Briefs ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BriefTrigger {
    /// The wall-clock schedule from Settings → Wire.
    #[default]
    Scheduled,
    /// Someone asked for one (`POST /briefs/generate`, or the `brief` MCP tool).
    Manual,
}

impl BriefTrigger {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "scheduled" => Some(Self::Scheduled),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// One cluster's entry in a brief. The prose is model-written; `story_id` and
/// `sources` are not, so the raw cluster stays inspectable underneath.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BriefItem {
    pub story_id: String,
    pub title: String,
    /// Two sentences plus why it matters.
    pub summary: String,
    pub sources: Vec<BriefSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BriefSource {
    pub article_id: String,
    pub source: String,
    /// The RAW url, for the same reason [`Article::url`] is the raw one.
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Brief {
    pub id: String,
    pub workspace_id: String,
    pub generated_at: i64,
    pub trigger: BriefTrigger,
    pub story_count: i64,
    pub article_count: i64,
    pub items: Vec<BriefItem>,
}

// ── Settings ───────────────────────────────────────────────────────────────────

/// How close two articles must be to count as the same copy.
///
/// The *choice* is a user preference (`news-dedupe-aggressiveness` in the manifest);
/// the *threshold* is code. Exposing "Hamming distance ≤ 3" in a settings dropdown
/// would ask a reader to have an opinion about a number they cannot evaluate.
// `ToSchema` because this rides inside the `PUT /api/news/settings` body, which Core
// lowers into an LLM tool: without it the `dedupe` argument has no allowed values and
// a model has to guess the three spellings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DedupeAggressiveness {
    /// Only near-identical copies.
    Relaxed,
    /// Catches syndicated wire copy. The ALGORITHMS default.
    #[default]
    Standard,
    /// Collapses rewrites too, at the cost of occasionally merging two genuinely
    /// different follow-ups.
    Aggressive,
}

impl DedupeAggressiveness {
    /// Maximum Hamming distance between two 64-bit SimHashes for the pair to count
    /// as the same copy.
    pub const fn hamming_threshold(self) -> u32 {
        match self {
            Self::Relaxed => 1,
            Self::Standard => 3,
            Self::Aggressive => 6,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Relaxed => "relaxed",
            Self::Standard => "standard",
            Self::Aggressive => "aggressive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "relaxed" => Some(Self::Relaxed),
            "standard" => Some(Self::Standard),
            "aggressive" => Some(Self::Aggressive),
            _ => None,
        }
    }
}

/// Per-workspace engine configuration, stored as one JSON blob.
///
/// ## Which side wins, and why four of these mirror a manifest preference
///
/// Five knobs have a home in the manifest's `settings_tabs` (`news-brief-time`,
/// `news-brief-timezone`, `news-brief-model`, `news-item-cap`,
/// `news-dedupe-aggressiveness`). Four of them are mirrored here; one deliberately
/// is not, and the difference is *who executes*:
///
/// - The poll loop and the brief scheduler run INSIDE this process on a timer, with
///   no request to carry a preference in. They cannot ask Core for a pref value —
///   there is no request to attach to and no reader for a pref in the host-API set
///   this sidecar is granted. So `brief_time`, `brief_timezone`, `item_cap` and
///   `dedupe` are mirrored into this blob, and **this blob is what the engine
///   obeys**. The settings tab is the editor; the companion pushes the value here on
///   save (`PUT /settings`). One winner, one sync direction, written down.
/// - `news-brief-model` is NOT mirrored, because the model call passes
///   [`crate::state::BRIEF_MODEL_PREF_KEY`] to Core's `/api/host/model/complete` and
///   Core resolves the preference itself. Copying it here would add a second value
///   that could disagree with the one actually used.
///
/// Everything else below has no manifest home at all: they are the spine's
/// constants, tunable for a power user but not worth a settings row.
// `ToSchema` because this IS the `PUT /api/news/settings` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(default)]
pub struct NewsSettings {
    /// Seconds between poll sweeps. The sidecar is `lazy` with a 15-minute idle stop,
    /// so this is also roughly how often Core is asked to keep it alive.
    pub poll_interval_secs: u64,
    /// Local wall-clock `HH:MM` the daily brief generates at. `None` = on demand only.
    pub brief_time: Option<String>,
    /// IANA zone the brief time and the burst hour-of-day buckets are read in. `None`
    /// = the node's own zone. A fixed UTC offset would be wrong for half the year in
    /// most of the world, which is why this is a zone NAME.
    pub brief_timezone: Option<String>,
    /// How many ranked stories a brief covers, and the cap on the KV snapshot the
    /// turn hook reads.
    pub item_cap: usize,
    /// How hard to collapse near-duplicate copies of the same article: `relaxed`
    /// (only near-identical), `standard`, or `aggressive` (collapses rewrites too).
    // Inlined so the three allowed values reach the model directly rather than as a
    // pointer it has to follow — Core resolves a `$ref` only one level in.
    #[schema(inline)]
    pub dedupe: DedupeAggressiveness,
    /// Clusters older than this are not considered as join candidates.
    pub cluster_window_hours: i64,
    /// Minimum blended similarity to join an existing cluster rather than open a new
    /// one.
    pub join_threshold: f64,
    /// Members after which a cluster's centroid freezes.
    pub centroid_k: i64,
    /// Recency half-life in the ranking formula.
    pub rank_half_life_hours: f64,
    /// Burst fires at or above this z-score …
    pub burst_z_threshold: f64,
    /// … AND at or above this absolute hourly count. The floor exists because a topic
    /// that normally sees zero articles an hour has a standard deviation of zero and
    /// would otherwise fire forever on a single article.
    pub burst_min_absolute: i64,
    /// Minimum gap between two `topic.breaking` emits for one topic.
    pub burst_cooldown_secs: i64,
    /// Articles older than this are pruned. `0` disables pruning.
    pub retention_days: i64,
}

impl Default for NewsSettings {
    fn default() -> Self {
        Self {
            poll_interval_secs: 900,
            brief_time: None,
            brief_timezone: None,
            item_cap: 25,
            dedupe: DedupeAggressiveness::Standard,
            cluster_window_hours: 72,
            join_threshold: 0.42,
            centroid_k: 20,
            rank_half_life_hours: 12.0,
            burst_z_threshold: 3.0,
            burst_min_absolute: 4,
            burst_cooldown_secs: 6 * 3600,
            retention_days: 90,
        }
    }
}

// ── The KV snapshot the turn hook reads ────────────────────────────────────────

/// The KV key the sidecar writes and `hooks/ground.js` reads.
///
/// Both sides omit the storage namespace, which Core resolves to the literal
/// `"default"` — they agree because neither names it, so do not add one on a single
/// side.
pub const SNAPSHOT_KEY: &str = "headlines.snapshot";

/// Snapshot schema version. Bump only alongside a hook update, since the hook is
/// what parses it.
pub const SNAPSHOT_VERSION: u32 = 1;

/// How long a snapshot stays usable, in seconds. Matches the hook's own fallback.
///
/// It matters because this sidecar is `lazy` with a 15-minute idle reap, so it is
/// usually stopped: without a TTL the last snapshot would sit in KV indefinitely and
/// the hook would ground today's question in last week's headlines while presenting
/// them as recent.
pub const SNAPSHOT_TTL_SECS: u64 = 5400;

/// The top-N headline snapshot handed to the `news.ground` turn hook.
///
/// **This is the one place in the crate where a timestamp is an RFC-3339 STRING**,
/// and it is not a style slip: the reader is a JavaScript sandbox fragment that does
/// `Date.parse(snap.generated_at)`, and a number there parses as milliseconds only
/// by accident of coercion for `items[].published_at`, which the hook renders
/// verbatim into the prompt. The field names below are a contract with a file on
/// disk (`apps-store/news/hooks/ground.js`) that no compiler checks — the test at
/// the bottom of this module is what does.
///
/// `items` ships in the sidecar's own rank order and the hook never re-sorts it.
/// `stopwords` and `tokens` ship WITH the snapshot on purpose: token matching only
/// works if both sides tokenize identically, and a word list copied into the hook
/// would be a second source of truth that drifts the first time the Rust side is
/// tuned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeadlineSnapshot {
    pub version: u32,
    /// RFC-3339 UTC.
    pub generated_at: String,
    pub ttl_secs: u64,
    /// The exact stopword list the sidecar used to produce `items[].tokens`.
    pub stopwords: Vec<String>,
    pub items: Vec<SnapshotItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotItem {
    pub id: String,
    pub title: String,
    pub source: String,
    /// The RAW url — what the user clicks.
    pub url: String,
    /// RFC-3339 UTC.
    pub published_at: String,
    pub story_id: Option<String>,
    pub source_count: i64,
    pub tokens: Vec<String>,
}

// ── Health ─────────────────────────────────────────────────────────────────────

/// What `/health` reports. Counts only — never user content — because the route is
/// un-gated: Core probes it before it has any reason to trust this process.
///
/// Producing it requires reading the database, which is the point: a health check
/// that only proves the process is alive answers 200 while every request 500s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCounts {
    pub sources: i64,
    pub articles: i64,
    pub stories: i64,
    pub topics: i64,
    pub briefs: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_carry_their_prefix() {
        assert!(new_id(ID_ARTICLE).starts_with("ar_"));
        assert!(new_id(ID_STORY).starts_with("st_"));
        assert_ne!(new_id(ID_TOPIC), new_id(ID_TOPIC));
    }

    #[test]
    fn rfc3339_is_utc_and_second_resolution() {
        // 2026-08-10T09:12:00Z, the instant the manifest's story.developing example
        // uses.
        assert_eq!(to_rfc3339(1_786_353_120_000), "2026-08-10T09:12:00Z");
        // An unrepresentable instant degrades to the epoch rather than panicking in
        // the middle of writing a snapshot.
        assert!(to_rfc3339(i64::MAX).starts_with("1970-01-01T00:00:00"));
    }

    #[test]
    fn enum_wire_values_round_trip_through_their_string_form() {
        for kind in [
            SourceKind::Rss,
            SourceKind::Atom,
            SourceKind::JsonFeed,
            SourceKind::Extract,
        ] {
            assert_eq!(SourceKind::parse(kind.as_str()), Some(kind));
        }
        for agg in [
            DedupeAggressiveness::Relaxed,
            DedupeAggressiveness::Standard,
            DedupeAggressiveness::Aggressive,
        ] {
            assert_eq!(DedupeAggressiveness::parse(agg.as_str()), Some(agg));
        }
        for trigger in [BriefTrigger::Scheduled, BriefTrigger::Manual] {
            assert_eq!(BriefTrigger::parse(trigger.as_str()), Some(trigger));
        }
        for mark in [ArticleMark::Read, ArticleMark::Saved, ArticleMark::Archived] {
            assert_eq!(ArticleMark::parse(mark.as_str()), Some(mark));
        }
        // An unknown value is `None`, so the row decoder can fall back to the
        // documented default rather than failing the whole list.
        assert_eq!(SourceKind::parse("gopher"), None);
    }

    #[test]
    fn dedupe_thresholds_are_ordered_by_aggressiveness() {
        assert!(
            DedupeAggressiveness::Relaxed.hamming_threshold()
                < DedupeAggressiveness::Standard.hamming_threshold()
        );
        assert!(
            DedupeAggressiveness::Standard.hamming_threshold()
                < DedupeAggressiveness::Aggressive.hamming_threshold()
        );
        assert_eq!(DedupeAggressiveness::default().hamming_threshold(), 3);
    }

    #[test]
    fn backoff_doubles_then_caps() {
        assert_eq!(backoff_hours(0), 0);
        assert_eq!(backoff_hours(1), 2);
        assert_eq!(backoff_hours(2), 4);
        assert_eq!(backoff_hours(4), 16);
        assert_eq!(backoff_hours(5), MAX_BACKOFF_HOURS);
        // The exponent is saturated, so a source that has failed for a year still
        // answers rather than overflowing the shift.
        assert_eq!(backoff_hours(1_000_000), MAX_BACKOFF_HOURS);
    }

    #[test]
    fn limits_are_clamped_into_range() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(10_000)), MAX_LIMIT);
        assert_eq!(clamp_limit(Some(7)), 7);
    }

    #[test]
    fn source_health_is_derived_from_the_failure_counter() {
        let mut source = Source {
            id: "src_1".into(),
            workspace_id: DEFAULT_WORKSPACE_ID.into(),
            title: "Reuters".into(),
            feed_url: "https://example.test/feed".into(),
            site_url: None,
            kind: SourceKind::Rss,
            enabled: true,
            etag: None,
            last_modified: None,
            last_fetch_at: None,
            last_success_at: None,
            consecutive_failures: 0,
            next_fetch_at: None,
            last_error: None,
            created_at: 0,
        };
        assert_eq!(source.health(), SourceHealth::Healthy);
        source.consecutive_failures = 1;
        assert_eq!(source.health(), SourceHealth::Degraded);
        source.consecutive_failures = FAILING_AFTER_CONSECUTIVE_FAILURES;
        assert_eq!(source.health(), SourceHealth::Failing);
    }

    /// The snapshot is parsed by `apps-store/news/hooks/ground.js`, which reads its
    /// fields BY NAME out of a JSON string. Nothing else checks that contract: a
    /// rename here compiles, ships, and makes the "Ground in news" toggle silently
    /// attach nothing forever, with no error anywhere to explain it.
    #[test]
    fn snapshot_serializes_the_field_names_the_turn_hook_reads() {
        let snapshot = HeadlineSnapshot {
            version: SNAPSHOT_VERSION,
            generated_at: to_rfc3339(1_786_353_120_000),
            ttl_secs: SNAPSHOT_TTL_SECS,
            stopwords: vec!["the".into()],
            items: vec![SnapshotItem {
                id: "ar_3402".into(),
                title: "Regulator opens inquiry into the merger".into(),
                source: "Reuters".into(),
                url: "https://example.test/a".into(),
                published_at: to_rfc3339(1_786_352_100_000),
                story_id: Some("st_9f2c".into()),
                source_count: 8,
                tokens: vec!["regulator".into(), "merger".into()],
            }],
        };
        let value = serde_json::to_value(&snapshot).unwrap();

        for key in ["version", "generated_at", "ttl_secs", "stopwords", "items"] {
            assert!(value.get(key).is_some(), "snapshot is missing `{key}`");
        }
        let item = &value["items"][0];
        for key in [
            "id",
            "title",
            "source",
            "url",
            "published_at",
            "story_id",
            "source_count",
            "tokens",
        ] {
            assert!(item.get(key).is_some(), "snapshot item is missing `{key}`");
        }
        // `Date.parse` must accept it, which means a string and not a number.
        assert!(value["generated_at"].is_string());
        assert!(item["published_at"].is_string());
    }
}
