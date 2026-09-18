//! Wire — a personal newsroom that owns its whole spine out of process.
//!
//! ```text
//!   feeds ──(fetch, conditional GET)──▶ parse ──▶ canonicalize URL ──▶ SimHash
//!                                                                        │
//!                                       cluster into stories ◀───────────┘
//!                                            │        │
//!                          topic match ──────┘        └────── rank ──▶ feed
//!                                │                                │
//!                          burst test                          brief ──(model)
//! ```
//!
//! A model is involved in exactly two places — two sentences per cluster in the
//! brief, and one neutral headline for a story eight outlets titled eight ways.
//! Everything a reader would be angry to see be wrong (what is a duplicate, what
//! belongs to which story, what counts as breaking, what a watch matches, what
//! sorts to the top) is computed, reproducible and inspectable. That is the same
//! split `@ryu/reasoning` draws, and it is the reason this crate hand-writes a feed
//! parser instead of pulling one in: the spine has to be ours to explain.
//!
//! # Why this crate has a `[lib]` at all
//!
//! It is NOT a Core-facing surface. Core links nothing here — it spawns this binary
//! and reaches it exclusively through the generic ext-proxy. The lib exists so the
//! deterministic spine is unit-testable and so the `mcp` subcommand and the HTTP
//! layer share one implementation, which is exactly why `ryu-reasoning` has the
//! same lib-plus-bin shape. Nothing outside this crate depends on it.
//!
//! # Layout
//!
//! | module | role |
//! |--------|------|
//! | [`paths`] | data-dir resolution, `RYU_DIR`-env-first (tracer copy) |
//! | [`models`] | the wire + domain types, the id/clock helpers |
//! | [`store`] | every SQL statement in the app |
//! | [`state`] | `AppState`, process config, the shared HTTP client, event ids |
//! | [`error`] | the one `ApiError` the whole HTTP surface returns |
//! | [`feed`] | a hand-written RSS 2.0 / Atom 1.0 tokenizer, plus JSON Feed |
//! | [`extract`] | a hand-written HTML-to-text pass with a density heuristic |
//! | [`text`] | tokenization, shingling and the stopword/entity primitives |
//! | [`canon`] | URL canonicalization — dedupe layer 1 |
//!
//! The remaining ingest half (the poll loop, the model callback, the HTTP router
//! and the MCP server) lands beside these as its own modules; this file is where
//! they are declared when they do.

pub mod api;
pub mod error;
pub mod host;
pub mod mcp;
pub mod models;
pub mod paths;
pub mod service;
pub mod state;
pub mod store;
pub mod tick;

// The deterministic spine. No model is involved in any of it, and none of it makes
// a network call — the same articles replay to the same clusters, the same burst
// verdicts and the same ranking, offline, on any machine.
pub mod burst;
pub mod canon;
pub mod cluster;
pub mod extract;
pub mod feed;
pub mod query;
pub mod rank;
pub mod simhash;
pub mod text;
