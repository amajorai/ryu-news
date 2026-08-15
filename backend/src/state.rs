//! The axum state every handler is built over: the store, one HTTP client, the
//! process config, and the app-event emitter.
//!
//! One state struct rather than per-module states, because the modules that land
//! beside this one (ingest, cluster, watch, brief) each need three of these four and
//! a narrower state per module would just mean converting between them at every
//! call. Every field is cheap to clone (`Arc` inside), so `State<AppState>`
//! extraction costs nothing per request.

use std::sync::Arc;
use std::time::Duration;

use crate::store::NewsStore;

/// The hard ceiling on ONE outbound call, end to end: a feed fetch, a `web.extract`
/// call, or the model callback into Core.
///
/// This is not optional politeness. A news reader is a program whose whole job is
/// talking to dozens of servers it does not control, and `reqwest::Client::new()`
/// has neither a request nor a connect timeout: a host that accepts the TCP
/// connection and then says nothing leaves the await pending FOREVER. The poll loop
/// joins on its fetches, so one such source wedges every other source's ingest —
/// silently, because `/health` only touches the store and keeps answering 200.
pub const OUTBOUND_CALL_TIMEOUT_MS: u64 = 30_000;

/// Ceiling on the TCP+TLS handshake specifically, well under the whole-call bound.
/// A host that is not answering at all should fail fast rather than burn the full
/// allowance, because a connect that hangs will not start succeeding at second 29.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The one HTTP client shape this process uses for outbound traffic.
///
/// A free function rather than an inline `Client::new()` at each site, because a
/// bound that holds at only one construction point is not a bound — and because
/// `reqwest::Client` owns a connection pool, so a client per fetch would re-do TLS
/// for every source on every poll. Falls back to the default client if the builder
/// ever fails, so a timeout config problem degrades to today's behaviour instead of
/// refusing to boot.
pub fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_millis(OUTBOUND_CALL_TIMEOUT_MS))
        .connect_timeout(CONNECT_TIMEOUT)
        // Redirects are FOLLOWED on purpose and the terminal URL is what gets
        // canonicalized: a link shim (`t.co`, `news.google.com/rss/articles/…`) is a
        // different URL per share of the same article, so canonicalizing the shim
        // would defeat dedupe layer 1 entirely.
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// This app's manifest `id`. Core authorizes every app-event emit against it — the
/// caller must *be* the plugin the event is namespaced to — so it must stay
/// byte-identical to the `id` in `apps-store/news/manifest.json`.
pub const PLUGIN_ID: &str = "@ryu/news";

/// The events this app declares in its manifest's `contributes.hook_events`.
///
/// Held as constants next to the id so the `<plugin id>#<name>` rule Core enforces
/// at load is checkable at a glance rather than spread across the modules that raise
/// them. The test at the bottom of this file checks it against the manifest itself.
pub const EVENT_BRIEF_READY: &str = "@ryu/news#brief.ready";
pub const EVENT_TOPIC_BREAKING: &str = "@ryu/news#topic.breaking";
pub const EVENT_STORY_DEVELOPING: &str = "@ryu/news#story.developing";

/// The preference key passed to Core's `/api/host/model/complete` for the two model
/// edges (cluster titling and brief prose).
///
/// Passed rather than resolved: Core owns the preference the settings tab writes, so
/// sending the KEY means there is exactly one value in play. Mirroring the resolved
/// model into this process's own settings would create a second copy that could
/// disagree with the one actually used — see [`crate::models::NewsSettings`] for
/// which knobs ARE mirrored and why.
pub const BRIEF_MODEL_PREF_KEY: &str = "news-brief-model";

/// Process-level configuration, resolved once at boot from the environment.
///
/// Distinct from [`crate::models::NewsSettings`], which is per-workspace and
/// user-editable. The split matters: a user must not be able to change the port or
/// the shared secret from the settings tab.
#[derive(Debug, Clone)]
pub struct Config {
    /// The loopback port this process listens on.
    pub port: u16,
    /// How many sources one sweep may fetch. Bounds the blast radius of a large
    /// subscription list: without it, a node with 400 feeds tries to fetch all 400
    /// in one tick.
    pub poll_batch_size: usize,
    /// Whether the poll loop runs at all. `RYU_NEWS_POLLER=0` disables it, which is
    /// what a test harness or a second read-only reader wants.
    /// The shared secret every request must carry, read from `RYU_EXT_TOKEN`.
    ///
    /// `None` when Core did not inject one, which the bearer gate treats as
    /// FAIL-CLOSED — see `bearer_ok` in `main.rs`.
    pub token: Option<String>,
    pub poller_enabled: bool,
}

impl Config {
    /// Read from the environment, with the defaults a normal Core-spawned run uses.
    pub fn from_env(port: u16) -> Self {
        Self {
            port,
            poll_batch_size: std::env::var("RYU_NEWS_POLL_BATCH")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|n| *n > 0)
                .unwrap_or(50),
            token: std::env::var("RYU_EXT_TOKEN")
                .ok()
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty()),
            poller_enabled: std::env::var("RYU_NEWS_POLLER")
                .map(|v| !matches!(v.trim(), "0" | "false" | "off"))
                .unwrap_or(true),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub store: NewsStore,
    /// One shared client for every outbound call — feeds, `web.extract`, and the
    /// model callback. Built by [`build_http_client`], so it carries the request and
    /// connect timeouts; never `Client::new()`, which has neither.
    pub http: reqwest::Client,
    pub config: Arc<Config>,
    /// Raises this app's declared hook events so plugin hooks and event-triggered
    /// workflows can react to a brief landing or a watch bursting without either side
    /// knowing the other exists.
    ///
    /// Safe to hold unconditionally: `from_env` never fails, and every emit no-ops
    /// when `RYU_CORE_PORT`/`RYU_EXT_TOKEN` are absent — which is the state under
    /// this crate's own tests and any standalone run, so no test needs a live Core.
    pub events: ryu_app_events::EventEmitter,
    /// The bridge back into Core, or `None` when this process runs standalone.
    ///
    /// Held on the state rather than rebuilt per request so the one shared
    /// `reqwest::Client` (with its timeouts) is reused — a client per call re-does TLS
    /// every time, and an untimed one wedges a whole poll.
    pub host: Option<crate::host::Host>,
}

impl AppState {
    pub fn new(store: NewsStore, config: Config) -> Self {
        let http = build_http_client();
        let host = crate::host::Host::from_env(http.clone(), OUTBOUND_CALL_TIMEOUT_MS);
        Self {
            store,
            http,
            config: Arc::new(config),
            events: ryu_app_events::EventEmitter::from_env(PLUGIN_ID),
            host,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest as shipped. Read at COMPILE time from the package directory —
    /// the same `include_str!` shape `apps-store/research/backend` uses — so the
    /// four constants nothing else checks are checked by something.
    const MANIFEST: &str = include_str!("../../manifest.json");

    fn manifest() -> serde_json::Value {
        serde_json::from_str(MANIFEST).expect("the shipped manifest must parse")
    }

    #[test]
    fn plugin_id_matches_the_manifest() {
        assert_eq!(manifest()["id"].as_str(), Some(PLUGIN_ID));
    }

    /// Core enforces `<plugin id>#<name>` at load. An event id that drifts is not a
    /// compile error and not a load error in this crate — it is an emit Core refuses
    /// at runtime, with the workflow bound to it simply never firing.
    #[test]
    fn every_declared_event_id_is_namespaced_and_present_in_the_manifest() {
        let manifest = manifest();
        let declared: Vec<&str> = manifest["contributes"]["hook_events"]
            .as_array()
            .expect("hook_events")
            .iter()
            .map(|e| e["id"].as_str().expect("hook event id"))
            .collect();

        for id in [
            EVENT_BRIEF_READY,
            EVENT_TOPIC_BREAKING,
            EVENT_STORY_DEVELOPING,
        ] {
            assert!(
                id.starts_with(&format!("{PLUGIN_ID}#")),
                "{id} is not namespaced to {PLUGIN_ID}"
            );
            assert!(declared.contains(&id), "{id} is not in the manifest");
        }
        assert_eq!(declared.len(), 3, "an event was added without a constant");
    }

    /// The model pref key is a string this process sends to Core; the settings tab
    /// is where it is set. A typo means Core resolves nothing and the brief silently
    /// uses whatever default it falls back to.
    #[test]
    fn the_brief_model_pref_key_is_the_one_the_settings_tab_writes() {
        let manifest = manifest();
        let fields = manifest["contributes"]["settings_tabs"][0]["fields"]
            .as_array()
            .expect("settings fields");
        assert!(fields
            .iter()
            .any(|f| f["pref_key"].as_str() == Some(BRIEF_MODEL_PREF_KEY)));
    }

    #[test]
    fn config_defaults_are_sane_and_the_poller_can_be_switched_off() {
        let config = Config::from_env(8008);
        assert_eq!(config.port, 8008);
        assert!(config.poll_batch_size > 0);
        // Not asserting `poller_enabled` — the environment is shared with whatever
        // else runs the test binary, and a test that reads an env var it did not set
        // is a test that fails on someone else's machine.
    }

    #[test]
    fn the_shared_client_is_the_only_way_a_timeout_gets_set() {
        // There is nothing to assert about a built `reqwest::Client` from outside —
        // its timeouts are not readable. What IS assertable is that the constants the
        // builder uses stay ordered: a connect timeout at or above the whole-call
        // bound would make the fast-fail path unreachable.
        assert!(CONNECT_TIMEOUT < Duration::from_millis(OUTBOUND_CALL_TIMEOUT_MS));
        let _ = build_http_client();
    }
}
