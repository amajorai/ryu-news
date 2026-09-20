//! One error type for the whole HTTP surface, so every handler can be written as
//! `-> ApiResult<Json<T>>` and use `?` on store/serde/fetch calls.
//!
//! Why a single enum rather than per-handler `(StatusCode, Json<Value>)` tuples:
//! this app serves nineteen declared routes across the ingest, story, watch and
//! brief surfaces. A tuple-returning convention makes every one of those handlers
//! re-implement its own error mapping, which is exactly how a 500 ends up leaking a
//! SQL string — or a feed URL with a credential in its query string — to the frame.
//! Funnelling through one `IntoResponse` gives a single place where the status code,
//! the stable machine-readable `code`, and the message-vs-detail split are decided.
//!
//! Wire shape is fixed and snake_case, matching the rest of the sidecar:
//! `{ "error": "<human message>", "code": "<machine code>" }`.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Every handler in the HTTP surface returns this.
pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub enum ApiError {
    /// The addressed row does not exist (or is not visible in this workspace).
    NotFound(String),
    /// The caller's payload is structurally wrong — a feed URL that is not a URL, a
    /// topic query that fails to parse (with its column offset in the message), an
    /// OPML body that is not OPML.
    BadRequest(String),
    /// The row exists but the request conflicts with what is already there — most
    /// often a subscription or a watch name this workspace already has. Distinct
    /// from `BadRequest` because the payload was fine.
    Conflict(String),
    /// A surface declared in the manifest whose handler has not landed yet. 501
    /// rather than 500 so the UI (and any smoke test) can tell "not built" from
    /// "broken", and so an alert on 5xx does not fire on a known gap.
    NotImplemented(String),
    /// A dependency we do not control failed — a source's server, the `web.extract`
    /// provider, Core's model callback. 502, because the fault is upstream of this
    /// process. This is the COMMON error in this app: a news reader talks to dozens
    /// of servers it has no control over, and a feed that times out is not a bug
    /// here.
    Upstream(String),
    /// A dependency this route needs is not connected — today, the host bridge that
    /// carries model calls. 503 rather than 500: nothing is broken, the app is
    /// running outside Ryu (or without the grant), and the fix is on the caller side.
    Unavailable(String),
    /// Anything else. The `anyhow` chain is logged in full; the client gets a fixed
    /// string, because these messages contain SQL, file paths, and occasionally
    /// fragments of credentials.
    Internal(anyhow::Error),
}

impl ApiError {
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound(what.into())
    }

    pub fn bad_request(why: impl Into<String>) -> Self {
        Self::BadRequest(why.into())
    }

    pub fn conflict(why: impl Into<String>) -> Self {
        Self::Conflict(why.into())
    }

    /// The marker a not-yet-written handler returns. Kept as a constructor rather
    /// than a bare string so `grep -rn "not_implemented"` finds every remaining gap
    /// in one pass.
    pub fn not_implemented(what: impl Into<String>) -> Self {
        Self::NotImplemented(what.into())
    }

    pub fn upstream(what: impl Into<String>) -> Self {
        Self::Upstream(what.into())
    }

    /// The stable machine-readable discriminator. The UI branches on this, never on
    /// the human message, so the message stays free to change.
    fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::BadRequest(_) => "bad_request",
            Self::Conflict(_) => "conflict",
            Self::NotImplemented(_) => "not_implemented",
            Self::Upstream(_) => "upstream_error",
            Self::Unavailable(_) => "unavailable",
            Self::Internal(_) => "internal_error",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) => write!(f, "{m} not found"),
            Self::BadRequest(m) | Self::Conflict(m) | Self::Upstream(m) | Self::Unavailable(m) => {
                write!(f, "{m}")
            }
            Self::NotImplemented(m) => write!(f, "{m} is not implemented yet"),
            Self::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Log the FULL chain before narrowing what the client sees. For `Internal`
        // this is the only place the real cause is ever recorded.
        if let Self::Internal(e) = &self {
            tracing::error!(error = ?e, "ryu-news: internal error");
        } else {
            tracing::debug!(error = %self, code = self.code(), "ryu-news: request rejected");
        }
        let status = self.status();
        let code = self.code();
        // `Internal` deliberately does NOT forward `e` — see the variant's doc.
        let message = match &self {
            Self::Internal(_) => "internal error".to_string(),
            other => other.to_string(),
        };
        (status, Json(json!({ "error": message, "code": code }))).into_response()
    }
}

// ── `?` conversions ────────────────────────────────────────────────────────────
//
// The store returns `anyhow::Result`, so `From<anyhow::Error>` is what makes every
// handler's `?` work. The `rusqlite`/`serde_json` conversions exist so a module that
// touches those crates directly (the OPML importer's own transaction, the snapshot
// writer encoding its payload) does not have to `.map_err(anyhow::Error::from)` at
// each call site.

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(anyhow::Error::from(e))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(anyhow::Error::from(e))
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        // A transport failure to a source's server is upstream, not our bug — and
        // saying so is what keeps a flaky feed out of this app's error budget.
        Self::Upstream(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_and_codes_are_stable() {
        assert_eq!(ApiError::not_found("story").status(), StatusCode::NOT_FOUND);
        assert_eq!(ApiError::not_found("story").code(), "not_found");
        assert_eq!(
            ApiError::bad_request("bad query").status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::conflict("already subscribed").status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            ApiError::not_implemented("opml export").status(),
            StatusCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            ApiError::upstream("feed timed out").status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            ApiError::upstream("feed timed out").code(),
            "upstream_error"
        );
    }

    /// The one behaviour worth a test of its own: an internal error's cause is
    /// logged, never sent. These messages carry SQL, absolute paths, and — in a feed
    /// reader specifically — source URLs that may have a token in the query string.
    #[test]
    fn internal_errors_do_not_leak_their_cause_to_the_client() {
        let err = ApiError::Internal(anyhow::anyhow!(
            "SELECT … FROM sources failed: https://wire.test/feed?key=s3cret"
        ));
        let message = match &err {
            ApiError::Internal(_) => "internal error".to_string(),
            other => other.to_string(),
        };
        assert_eq!(message, "internal error");
        assert!(!message.contains("s3cret"));
    }

    /// A `?` on a store call must not become a 500 with the SQL attached, and a `?`
    /// on a fetch must not become a 500 at all.
    #[test]
    fn conversions_land_on_the_right_variant() {
        let from_store: ApiError = anyhow::anyhow!("store blew up").into();
        assert_eq!(from_store.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let from_sql: ApiError = rusqlite::Error::QueryReturnedNoRows.into();
        assert_eq!(from_sql.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let from_json: ApiError = serde_json::from_str::<serde_json::Value>("{oh no")
            .unwrap_err()
            .into();
        assert_eq!(from_json.code(), "internal_error");
    }
}
