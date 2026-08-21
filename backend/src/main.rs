//! Process shell for `ryu-news`.
//!
//! Three modes, decided before anything binds a port:
//!
//! - `ryu-news mcp` — serve the MCP protocol on stdin/stdout and exit. **Nothing
//!   binds a listener in this mode**, so several MCP clients can run their own copy.
//! - `ryu-news` — serve HTTP on loopback, behind the bearer gate.
//! - either — the store is opened and migrated first, so a schema problem is a
//!   startup failure rather than a 500 on the first request.
//!
//! # The four constants
//!
//! Two of these must match `apps-store/news/manifest.json` byte-for-byte and
//! **nothing checks them at compile time** (the other two, the plugin id and the
//! event ids, live in `state.rs` and are checked there):
//!
//! - [`DEFAULT_PORT`] == `sidecars[0].port`. Drift means Core probes a port nobody is
//!   listening on and reports the sidecar unhealthy while it happily serves.
//! - [`MOUNT`] == `sidecars[0].http.mount`. Drift means Core forwards
//!   `/api/news/skills` to a router that only knows `/skills`.
//!
//! `api.rs` asserts both against the manifest, which is the only reason they are safe.
//!
//! # No graceful shutdown, deliberately
//!
//! Core owns this process's lifecycle. What the shutdown path DOES do is abort the
//! background tick before returning, so a supervised restart never briefly runs two
//! ticks against one database.

use std::net::{Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::Response,
    routing::get,
    Json, Router,
};
use serde_json::json;
use tracing_subscriber::EnvFilter;

use ryu_news::{
    api, mcp, paths,
    state::{AppState, Config},
    store::NewsStore,
    tick,
};

/// Loopback port. 8004/8005/8006/8007 are taken by `@ryu/ugc`, `@ryu/social`,
/// `@ryu/reasoning` and `@ryu/tuition`; there is no port registry, so this was picked
/// by reading the other manifests. Core injects `RYU_NEWS_PORT` (profile-shifted) at
/// spawn and this is only the standalone fallback.
const DEFAULT_PORT: u16 = 8008;

/// Everything this process serves lives under here, matching the manifest's
/// `http.mount`. `/health` is the one route outside it.
const MOUNT: &str = "/api/news";

/// The shared secret Core mints for this sidecar.
const ENV_TOKEN: &str = "RYU_EXT_TOKEN";

#[tokio::main]
async fn main() -> Result<()> {
    // stderr, always: in `mcp` mode stdout carries the JSON-RPC stream, and one log
    // line on stdout desynchronizes framing for every later frame.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let port: u16 = std::env::var("RYU_NEWS_PORT")
        .ok()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let db_path = paths::ryu_dir().join("news.db");
    let store = NewsStore::open(db_path.clone())
        .with_context(|| format!("opening {}", db_path.display()))?;
    let state = AppState::new(store, Config::from_env(port));

    // Dispatched AFTER the state exists and BEFORE any listener binds.
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        // The host bridge rides on `state`, so unlike tuition's this takes one arg.
        return mcp::serve(state).await;
    }

    let ticker = if state.config.poller_enabled {
        Some(tokio::spawn(tick::run(state.clone())))
    } else {
        tracing::info!("news: the poll loop is disabled by configuration");
        None
    };

    // The gated half: everything under the mount, behind the bearer middleware.
    // `api::routes` already applied the state, so this is a `Router<()>`.
    //
    // `/openapi.json` rides INSIDE that same gate, at the SERVER ROOT. Core fetches
    // `http://127.0.0.1:<port>/openapi.json` on this sidecar's first Healthy edge and
    // lowers every operation it finds into a searchable LLM tool, so routing this one
    // endpoint is what makes the whole `/api/news` surface callable by an agent — tool
    // derivation reads the document, never the router. The stdio `mcp` subcommand
    // covers four read verbs and nothing that writes, so this is the only path by which
    // subscribing, saving, or saving a topic watch becomes reachable.
    //
    // Root, not under the mount: Core tries the root FIRST, and keeping the document
    // off the mount keeps it out of the manifest's declared `http.routes[]` — anything
    // declared there is reachable through the generic ext-proxy, and this schema is
    // Core's to read, not an app surface. Inside the gate, not next to the un-gated
    // `/health`: Core stamps the injected `RYU_EXT_TOKEN` on the fetch, so the gate
    // costs the fetcher nothing, while un-gated it would disclose this app's entire
    // internal API surface to any other process on loopback. The handler closes over
    // no state, so `gated` stays a `Router<()>` and the merge below is unaffected.
    let gated = Router::new()
        .nest(MOUNT, api::routes(state.clone()))
        .route("/openapi.json", get(|| async { Json(api::openapi()) }))
        .layer(from_fn_with_state(state.clone(), bearer_gate));

    let app = Router::new()
        // `/health` sits on the OUTER router, UNGATED: Core probes it before it has
        // any reason to trust this process, and a gated health check would report a
        // healthy sidecar as down for the whole of its first second. Its state is
        // applied here so both halves are `Router<()>` before they merge.
        .route("/health", get(health))
        .with_state(state)
        .merge(gated);

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(port, mount = MOUNT, db = %db_path.display(), "ryu-news: listening");

    let served = axum::serve(listener, app).await;

    // Abort BEFORE returning the serve result, so a supervised restart cannot briefly
    // run two ticks against one database.
    if let Some(handle) = ticker {
        handle.abort();
    }
    served.context("the HTTP server stopped")?;
    Ok(())
}

/// Liveness AND readiness: the store must answer.
///
/// A health check that only proves the process is running is worse than none — Core
/// would report a sidecar with an unreadable database as healthy and route traffic
/// into 500s.
async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    match state.store.health_counts().await {
        Ok(counts) => Ok(Json(json!({
            "ok": true,
            "sources": counts.sources,
            "articles": counts.articles,
            "stories": counts.stories,
            "topics": counts.topics,
            "briefs": counts.briefs,
        }))),
        Err(err) => {
            tracing::error!(error = %err, "news: health check could not read the store");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// Loopback (belt) plus a shared-secret bearer (suspenders).
async fn bearer_gate(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let provided = bearer_of(request.headers());
    if bearer_ok(provided.as_deref(), state.config.token.as_deref()) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn bearer_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned)
}

/// The gate decision, as a pure function so it is testable without an axum request.
///
/// **Fail-closed**: no configured token rejects everything. The tempting alternative —
/// "no token means no auth, allow it" — turns a misconfigured spawn into an open
/// endpoint on loopback, reachable by every process on the machine.
///
/// The comparison is constant-time over the bytes. The secret is loopback-only and an
/// attacker who can time it can usually just read it, but a variable-time compare on a
/// credential is the kind of thing that gets copied into somewhere it matters.
#[must_use]
pub fn bearer_ok(provided: Option<&str>, expected: Option<&str>) -> bool {
    let (Some(provided), Some(expected)) = (provided, expected) else {
        return false;
    };
    if expected.is_empty() || provided.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in provided.bytes().zip(expected.bytes()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Whether a token is configured at all, for the startup warning.
#[must_use]
pub fn token_configured() -> bool {
    std::env::var(ENV_TOKEN)
        .ok()
        .is_some_and(|t| !t.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_is_fail_closed_without_a_configured_token() {
        // The whole point. "No token configured" must never mean "no auth required":
        // that turns a misconfigured spawn into an open endpoint every process on the
        // machine can reach.
        assert!(!bearer_ok(Some("anything"), None));
        assert!(!bearer_ok(None, None));
        assert!(!bearer_ok(Some(""), Some("")));
    }

    #[test]
    fn a_missing_or_wrong_header_is_refused() {
        assert!(!bearer_ok(None, Some("secret")));
        assert!(!bearer_ok(Some("wrong"), Some("secret")));
        assert!(!bearer_ok(Some(""), Some("secret")));
    }

    #[test]
    fn the_right_token_passes() {
        assert!(bearer_ok(Some("secret"), Some("secret")));
    }

    #[test]
    fn a_prefix_of_the_token_does_not_pass() {
        // The length check is what makes this true, and it is also what makes the
        // byte loop safe to write as a zip.
        assert!(!bearer_ok(Some("sec"), Some("secret")));
        assert!(!bearer_ok(Some("secretsecret"), Some("secret")));
    }

    #[test]
    fn the_bearer_prefix_is_required_and_case_sensitive() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer abc".parse().expect("a valid header"),
        );
        assert_eq!(bearer_of(&headers).as_deref(), Some("abc"));

        let mut bare = HeaderMap::new();
        bare.insert(
            axum::http::header::AUTHORIZATION,
            "abc".parse().expect("a valid header"),
        );
        assert!(bearer_of(&bare).is_none());
    }

    #[test]
    fn the_mount_matches_the_manifest() {
        // One of the four constants nothing else checks. `api.rs` asserts the route
        // table against the same file; this asserts the prefix they hang off.
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../../manifest.json")).expect("valid JSON");
        assert_eq!(manifest["sidecars"][0]["http"]["mount"], MOUNT);
        assert_eq!(manifest["sidecars"][0]["port"], DEFAULT_PORT);
        assert_eq!(
            manifest["sidecars"][0]["process"]["port_env"],
            "RYU_NEWS_PORT"
        );
    }
}
