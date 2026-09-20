//! Calling back into Core: the side-model edge, and the KV snapshot the turn hook reads.
//!
//! Two callbacks, both over loopback, both authenticated with the token Core mints for
//! this sidecar at spawn:
//!
//! ```text
//! POST http://127.0.0.1:$RYU_CORE_PORT/api/host/model/complete
//! POST http://127.0.0.1:$RYU_CORE_PORT/api/host/rpc          { method, args }
//!   authorization: Bearer $RYU_EXT_TOKEN
//!   x-ryu-plugin-id: $RYU_EXT_PLUGIN_ID
//! ```
//!
//! # Why the KV snapshot exists
//!
//! The "Ground in news" turn hook runs in Core's Deno sandbox, which has **no HTTP**.
//! It cannot query this sidecar, and there is no seam that would let it — that is a
//! deliberate property of the sandbox, not an oversight to route around.
//!
//! What both sides *can* reach is `storage.*`, in the kernel-contracts host-API table
//! under the `storage:kv` grant. So the flow runs the OPPOSITE way to
//! `@ryu/news`'s: this process PUBLISHES a compact, already-ranked headline
//! snapshot after each poll, and the hook reads that one key and token-matches against
//! it. The hook does no ranking of its own, which is why the snapshot carries the
//! tokens and the stopword list rather than the raw text — a second tokenizer in JS
//! would drift from [`crate::text`] and the two would disagree about what matched.
//!
//! One key, overwritten, never a queue: the hook wants the current state of the world,
//! and a backlog of stale snapshots is worse than none.
//!
//! # Absent host
//!
//! [`Host::from_env`] returns `None` when any of the three environment variables is
//! missing, which is the normal state when this binary runs standalone (its own tests
//! do exactly that). Model-backed routes then answer 503 with a message that says what
//! to do, and the snapshot publish becomes a no-op rather than an error that fails the
//! poll that produced it.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

const ENV_TOKEN: &str = "RYU_EXT_TOKEN";
const ENV_PLUGIN_ID: &str = "RYU_EXT_PLUGIN_ID";
const ENV_CORE_PORT: &str = "RYU_CORE_PORT";

/// The single KV key holding the headline snapshot.
///
/// Must match `hooks/ground.js` exactly. Nothing checks it at compile time — the two
/// sides are a JS fragment and a Rust binary — so it is stated in both files and in
/// the manifest's hook description.
pub const SNAPSHOT_KEY: &str = "news/snapshot/current";

/// The host bridge, when this process is Core-hosted.
#[derive(Debug, Clone)]
pub struct Host {
    base: String,
    token: String,
    plugin_id: String,
    http: reqwest::Client,
    timeout: Duration,
}

impl Host {
    /// Build a bridge from the environment Core injects at spawn, or `None` when this
    /// process is running standalone.
    #[must_use]
    pub fn from_env(http: reqwest::Client, timeout_ms: u64) -> Option<Host> {
        let token = non_empty(ENV_TOKEN)?;
        let plugin_id = non_empty(ENV_PLUGIN_ID)?;
        let port = non_empty(ENV_CORE_PORT)?;
        Some(Host {
            base: format!("http://127.0.0.1:{port}"),
            token,
            plugin_id,
            http,
            timeout: Duration::from_millis(timeout_ms),
        })
    }

    /// One side-model completion. Requires the `hook:side-model` grant in BOTH the
    /// sidecar's `host_api.grants` and the top-level `permission_grants` — the host
    /// authorizes on declared ∩ Gateway-approved, so one alone is a runtime 403 with
    /// nothing at parse time to explain it.
    pub async fn complete(
        &self,
        system: &str,
        prompt: &str,
        model_pref_key: Option<&str>,
    ) -> Result<String> {
        let mut body = json!({ "system": system, "prompt": prompt });
        if let Some(key) = model_pref_key {
            body["model_pref_key"] = json!(key);
        }
        let response = self
            .http
            .post(format!("{}/api/host/model/complete", self.base))
            .bearer_auth(&self.token)
            .header("x-ryu-plugin-id", &self.plugin_id)
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .context("the model callback to Core failed")?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .context("Core's model response was not JSON")?;
        if !status.is_success() {
            let detail = payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no detail");
            return Err(anyhow!("Core refused the model call ({status}): {detail}"));
        }
        payload
            .get("text")
            .or_else(|| payload.get("content"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("Core's model response carried no text"))
    }

    /// One host-API method over `/api/host/rpc`.
    pub async fn rpc(&self, method: &str, args: Value) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}/api/host/rpc", self.base))
            .bearer_auth(&self.token)
            .header("x-ryu-plugin-id", &self.plugin_id)
            .timeout(self.timeout)
            .json(&json!({ "method": method, "args": args }))
            .send()
            .await
            .with_context(|| format!("the '{method}' host call failed"))?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .with_context(|| format!("Core's '{method}' response was not JSON"))?;
        if !status.is_success() {
            let detail = payload
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no detail");
            return Err(anyhow!("Core refused '{method}' ({status}): {detail}"));
        }
        Ok(payload.get("result").cloned().unwrap_or(payload))
    }

    /// Publish the headline snapshot the "Ground in news" hook reads.
    ///
    /// Best-effort by contract: a failure here must never fail the poll that produced
    /// the snapshot. The hook already treats a missing or stale key as "attach
    /// nothing", so the degraded mode is a turn with no news context — which is
    /// exactly what the user gets with the toggle off, and strictly better than a
    /// failed ingest.
    pub async fn publish_snapshot(&self, snapshot: &crate::models::HeadlineSnapshot) -> bool {
        let payload = match serde_json::to_value(snapshot) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(error = %err, "news: snapshot did not serialize");
                return false;
            }
        };
        match self
            .rpc(
                "storage.set",
                json!({ "key": SNAPSHOT_KEY, "value": payload }),
            )
            .await
        {
            Ok(_) => true,
            Err(err) => {
                tracing::warn!(error = %err, "news: snapshot publish failed; the hook will attach nothing");
                false
            }
        }
    }
}

fn non_empty(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// The message a model-backed route returns when there is no host.
///
/// Actionable on purpose: "unavailable" sends someone to the logs, this sends them to
/// the right place.
#[must_use]
pub fn no_host_message() -> &'static str {
    "this needs a model, and the app is not connected to Ryu right now — \
     open it from the Ryu desktop app rather than running the binary directly"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize the env-var mutations: these tests share one process environment and
    /// `cargo test` runs them on separate threads.
    fn with_env<T>(vars: &[(&str, Option<&str>)], body: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| ((*k).to_owned(), std::env::var(k).ok()))
            .collect();
        for (key, value) in vars {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        let out = body();
        for (key, value) in saved {
            match value {
                Some(v) => std::env::set_var(&key, v),
                None => std::env::remove_var(&key),
            }
        }
        out
    }

    #[test]
    fn a_standalone_process_has_no_host_rather_than_a_broken_one() {
        // Running outside Core is a SUPPORTED state — this crate's own tests do it —
        // so it must not be a startup error, and it must not produce a bridge that
        // builds doomed requests.
        with_env(
            &[
                (ENV_TOKEN, None),
                (ENV_PLUGIN_ID, None),
                (ENV_CORE_PORT, None),
            ],
            || {
                assert!(Host::from_env(reqwest::Client::new(), 1000).is_none());
            },
        );
    }

    #[test]
    fn a_partially_configured_host_is_no_host_at_all() {
        // Two of three set is not "mostly working": every call would 401. Failing to
        // build the bridge turns that into one clear 503 instead.
        with_env(
            &[
                (ENV_TOKEN, Some("t")),
                (ENV_PLUGIN_ID, Some("@ryu/news")),
                (ENV_CORE_PORT, None),
            ],
            || assert!(Host::from_env(reqwest::Client::new(), 1000).is_none()),
        );
    }

    #[test]
    fn whitespace_only_values_do_not_count_as_configured() {
        with_env(
            &[
                (ENV_TOKEN, Some("   ")),
                (ENV_PLUGIN_ID, Some("@ryu/news")),
                (ENV_CORE_PORT, Some("8080")),
            ],
            || assert!(Host::from_env(reqwest::Client::new(), 1000).is_none()),
        );
    }

    #[test]
    fn a_fully_configured_host_builds_a_loopback_base() {
        with_env(
            &[
                (ENV_TOKEN, Some("t")),
                (ENV_PLUGIN_ID, Some("@ryu/news")),
                (ENV_CORE_PORT, Some("8980")),
            ],
            || {
                let host = Host::from_env(reqwest::Client::new(), 1000).expect("configured");
                // Loopback only. A sidecar must never be reachable off-box.
                assert_eq!(host.base, "http://127.0.0.1:8980");
            },
        );
    }

    #[test]
    fn the_snapshot_key_is_namespaced_to_this_app() {
        // The hook and this binary agree on this string with nothing checking it, so
        // at minimum it must be scoped so a collision with another app is impossible.
        assert!(SNAPSHOT_KEY.starts_with("news/"));
    }

    #[test]
    fn the_no_host_message_tells_the_reader_what_to_do() {
        let message = no_host_message();
        assert!(message.contains("Ryu"), "{message}");
        assert!(message.len() > 40, "too terse to act on: {message}");
    }
}
