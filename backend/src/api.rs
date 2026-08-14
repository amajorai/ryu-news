//! The HTTP surface, mounted at `/api/news`.
//!
//! Handlers are thin: anything that decides something lives in [`crate::service`] or
//! the engine modules, so the companion and the MCP server cannot drift apart. What is
//! left here is extraction, the JSON envelope, and turning a missing row into a 404.
//!
//! # Every route here must also be in the manifest
//!
//! Core's ext-proxy matches the declared `http.routes[]` with an EXACT segment count,
//! so a route this router serves but the manifest does not declare is a hard 404 that
//! reads like a bug in this file. Both directions are asserted at the bottom.

use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    error::{ApiError, ApiResult},
    models::{now_ms, ArticleMark, ArticleQuery, BriefTrigger, NewSource, NewsSettings, SourceKind},
    query, service,
    state::AppState,
};

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 500;

/// Every path this router serves, relative to the mount.
///
/// Duplicated from the `.route()` calls rather than derived, because axum's `Router`
/// does not expose its table. The manifest cross-check reads this list, so a route
/// added below but not here is caught by the same test that catches a missing manifest
/// entry.
pub const SERVED_ROUTES: &[&str] = &[
    "/sources",
    "/sources/opml",
    "/sources/:id",
    "/sources/:id/refresh",
    "/articles",
    "/articles/:id",
    "/articles/:id/read",
    "/articles/:id/save",
    "/articles/:id/archive",
    "/stories",
    "/stories/:id",
    "/stories/:id/follow",
    "/topics",
    "/topics/:id",
    "/topics/:id/matches",
    "/briefs",
    "/briefs/generate",
    "/briefs/:id",
    "/settings",
];

pub fn routes(state: AppState) -> Router<()> {
    Router::new()
        .route("/sources", get(list_sources).post(create_source))
        .route("/sources/opml", get(export_opml).post(import_opml))
        .route("/sources/:id", get(get_source).patch(update_source).delete(delete_source))
        .route("/sources/:id/refresh", post(refresh_source))
        .route("/articles", get(list_articles))
        .route("/articles/:id", get(get_article))
        .route("/articles/:id/read", post(mark_read))
        .route("/articles/:id/save", post(mark_saved))
        .route("/articles/:id/archive", post(mark_archived))
        .route("/stories", get(list_stories))
        .route("/stories/:id", get(get_story))
        .route("/stories/:id/follow", post(follow_story))
        .route("/topics", get(list_topics).post(create_topic))
        .route("/topics/:id", get(get_topic).patch(update_topic).delete(delete_topic))
        .route("/topics/:id/matches", get(topic_matches))
        .route("/briefs", get(list_briefs))
        .route("/briefs/generate", post(generate_brief))
        .route("/briefs/:id", get(get_brief))
        .route("/settings", get(get_settings).put(put_settings))
        .with_state(state)
}

/// The workspace every request operates on.
///
/// Single-workspace today, resolved rather than taken as a parameter so the routes do
/// not have to carry an id nobody chooses yet — and so introducing a second workspace
/// later is a change here rather than in nineteen handlers.
async fn workspace(state: &AppState) -> ApiResult<String> {
    let workspaces = state.store.list_workspaces().await?;
    workspaces
        .first()
        .map(|w| w.id.clone())
        .ok_or_else(|| ApiError::NotFound("workspace".into()))
}

fn require_hit(changed: bool, what: &str) -> ApiResult<Json<Value>> {
    if changed {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err(ApiError::NotFound(what.to_owned()))
    }
}

// ── Sources ────────────────────────────────────────────────────────────────────

async fn list_sources(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    Ok(Json(json!({ "sources": state.store.list_sources(&workspace_id).await? })))
}

#[derive(Debug, Deserialize)]
struct SourceBody {
    title: Option<String>,
    feed_url: String,
    site_url: Option<String>,
    kind: Option<String>,
    enabled: Option<bool>,
}

async fn create_source(
    State(state): State<AppState>,
    Json(body): Json<SourceBody>,
) -> ApiResult<Json<Value>> {
    if !crate::canon::is_http_url(&body.feed_url) {
        return Err(ApiError::BadRequest(
            "a source needs an http or https feed URL".into(),
        ));
    }
    let workspace_id = workspace(&state).await?;
    let created = state
        .store
        .create_source(
            &workspace_id,
            &NewSource {
                // The feed's own title is the better default, but it is not known
                // until the first fetch. The URL is a placeholder the first poll
                // replaces, not a name anybody has to live with.
                title: body.title.unwrap_or_else(|| body.feed_url.clone()),
                feed_url: body.feed_url.clone(),
                site_url: body.site_url,
                // Unknown or absent: the first fetch decides. `SourceKind::parse`
                // returns `None` for anything it does not recognise, and guessing
                // wrong here would be overwritten by the poll anyway.
                kind: SourceKind::parse(body.kind.as_deref().unwrap_or(""))
                    .unwrap_or(SourceKind::Rss),
            },
        )
        .await?;
    match created {
        Some(source) => Ok(Json(serde_json::to_value(source)?)),
        // A duplicate feed URL is a 409, not a silent success: the caller asked to add
        // something and needs to know it was already there.
        None => Err(ApiError::Conflict("that feed is already a source".into())),
    }
}

async fn get_source(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let source = state
        .store
        .get_source(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound("source".into()))?;
    Ok(Json(serde_json::to_value(source)?))
}

async fn update_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SourceBody>,
) -> ApiResult<Json<Value>> {
    let changed = state
        .store
        .update_source(
            &id,
            body.title.as_deref().unwrap_or_default(),
            body.site_url.as_deref(),
            body.enabled.unwrap_or(true),
        )
        .await?;
    require_hit(changed, "source")
}

async fn delete_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_hit(state.store.delete_source(&id).await?, "source")
}

async fn refresh_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let source = state
        .store
        .get_source(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound("source".into()))?;
    let report = service::refresh_one(&state, &source, now_ms()).await?;
    Ok(Json(serde_json::to_value(report)?))
}

// ── OPML ───────────────────────────────────────────────────────────────────────

async fn export_opml(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    let sources = state.store.list_sources(&workspace_id).await?;
    Ok(Json(json!({ "opml": service::to_opml(&sources) })))
}

#[derive(Debug, Deserialize)]
struct OpmlBody {
    opml: String,
}

async fn import_opml(
    State(state): State<AppState>,
    Json(body): Json<OpmlBody>,
) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    let (added, skipped) = service::import_opml(&state, &workspace_id, &body.opml).await?;
    Ok(Json(json!({ "added": added, "skipped": skipped })))
}

// ── Articles ───────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct ArticleParams {
    pub story_id: Option<String>,
    pub source_id: Option<String>,
    pub unread: Option<bool>,
    pub saved: Option<bool>,
    pub archived: Option<bool>,
    pub since_hours: Option<i64>,
    pub limit: Option<usize>,
}

async fn list_articles(
    State(state): State<AppState>,
    Query(params): Query<ArticleParams>,
) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    let now = now_ms();
    let settings = state.store.get_settings(&workspace_id).await?;
    let articles = state
        .store
        .list_articles(
            &workspace_id,
            &ArticleQuery {
                story_id: params.story_id.clone(),
                source_id: params.source_id.clone(),
                unread: params.unread,
                saved: params.saved,
                archived: params.archived,
                since: params.since_hours.map(|h| now - h.clamp(1, 24 * 365) * 3_600_000),
                limit: Some(params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)),
                ..Default::default()
            },
        )
        .await?;

    // Ranked here rather than in SQL, and the FACTORS ride along: the feed is expected
    // to answer "why is this at the top", and a score computed in one place and
    // explained in another is how the two come to disagree.
    let ranked = service::rank_articles(&articles, now, settings.rank_half_life_hours);
    let items: Vec<Value> = ranked
        .into_iter()
        .map(|(article, factors)| {
            let mut value = serde_json::to_value(article).unwrap_or(Value::Null);
            value["rank"] = json!({
                "total": factors.total,
                "recency": factors.recency,
                "coverage": factors.coverage,
                "topic": factors.topic,
                "unread": factors.unread,
            });
            value
        })
        .collect();
    Ok(Json(json!({ "articles": items })))
}

async fn get_article(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let article = state
        .store
        .get_article(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound("article".into()))?;
    Ok(Json(serde_json::to_value(article)?))
}

#[derive(Debug, Deserialize)]
struct MarkBody {
    #[serde(default = "yes")]
    on: bool,
}

const fn yes() -> bool {
    true
}

async fn mark(state: &AppState, id: &str, mark: ArticleMark, on: bool) -> ApiResult<Json<Value>> {
    require_hit(
        state.store.set_article_mark(id, mark, on, now_ms()).await?,
        "article",
    )
}

async fn mark_read(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<MarkBody>>,
) -> ApiResult<Json<Value>> {
    mark(&state, &id, ArticleMark::Read, body.map_or(true, |b| b.on)).await
}

async fn mark_saved(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<MarkBody>>,
) -> ApiResult<Json<Value>> {
    mark(&state, &id, ArticleMark::Saved, body.map_or(true, |b| b.on)).await
}

async fn mark_archived(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<MarkBody>>,
) -> ApiResult<Json<Value>> {
    mark(&state, &id, ArticleMark::Archived, body.map_or(true, |b| b.on)).await
}

// ── Stories ────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct StoryParams {
    pub since_hours: Option<i64>,
    pub limit: Option<usize>,
}

async fn list_stories(
    State(state): State<AppState>,
    Query(params): Query<StoryParams>,
) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    let since = params
        .since_hours
        .map(|h| now_ms() - h.clamp(1, 24 * 365) * 3_600_000);
    let stories = state
        .store
        .list_stories(
            &workspace_id,
            since,
            params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        )
        .await?;
    Ok(Json(json!({ "stories": stories })))
}

async fn get_story(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    let story = state
        .store
        .get_story(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound("story".into()))?;
    let articles = state
        .store
        .list_articles(
            &workspace_id,
            &ArticleQuery {
                story_id: Some(id),
                limit: Some(100),
                // Duplicates included: "who else ran this" is the question a story
                // page exists to answer, and a syndicated copy is a real answer.
                include_duplicates: true,
                ..Default::default()
            },
        )
        .await?;
    Ok(Json(json!({ "story": story, "articles": articles })))
}

#[derive(Debug, Deserialize)]
struct FollowBody {
    #[serde(default = "yes")]
    followed: bool,
}

async fn follow_story(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<FollowBody>>,
) -> ApiResult<Json<Value>> {
    let followed = body.map_or(true, |b| b.followed);
    require_hit(state.store.set_story_followed(&id, followed).await?, "story")
}

// ── Topics ─────────────────────────────────────────────────────────────────────

async fn list_topics(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    Ok(Json(json!({ "topics": state.store.list_topics(&workspace_id).await? })))
}

#[derive(Debug, Deserialize)]
struct TopicBody {
    name: String,
    query: String,
    enabled: Option<bool>,
}

/// Parse a topic query, turning a failure into a 400 that carries the COLUMN.
///
/// This is the whole design of the query language showing through the API: a watch
/// that silently matches nothing is worse than one that refuses to save, so the error
/// has to reach the user precisely enough to fix.
fn compile(raw: &str) -> ApiResult<(Value, query::Node)> {
    let node = query::parse(raw).map_err(|err| {
        ApiError::BadRequest(format!("{} (column {})", err.message, err.column))
    })?;
    let ast = serde_json::to_value(&node)?;
    Ok((ast, node))
}

async fn create_topic(
    State(state): State<AppState>,
    Json(body): Json<TopicBody>,
) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    let (ast, _) = compile(&body.query)?;
    match state
        .store
        .create_topic(&workspace_id, &body.name, &body.query, &ast)
        .await?
    {
        Some(topic) => Ok(Json(serde_json::to_value(topic)?)),
        None => Err(ApiError::Conflict("a topic with that name exists".into())),
    }
}

async fn get_topic(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let topic = state
        .store
        .get_topic(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound("topic".into()))?;
    Ok(Json(serde_json::to_value(topic)?))
}

async fn update_topic(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TopicBody>,
) -> ApiResult<Json<Value>> {
    let (ast, _) = compile(&body.query)?;
    let changed = state
        .store
        .update_topic(&id, &body.name, &body.query, &ast, body.enabled.unwrap_or(true))
        .await?;
    require_hit(changed, "topic")
}

async fn delete_topic(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_hit(state.store.delete_topic(&id).await?, "topic")
}

async fn topic_matches(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<StoryParams>,
) -> ApiResult<Json<Value>> {
    let now = now_ms();
    let from = now - params.since_hours.unwrap_or(168).clamp(1, 24 * 365) * 3_600_000;
    let articles = state
        .store
        .topic_match_articles(&id, from, now, params.limit.unwrap_or(100).clamp(1, MAX_LIMIT))
        .await?;
    let last_burst = state.store.last_burst(&id).await?;
    Ok(Json(json!({ "articles": articles, "last_burst": last_burst })))
}

// ── Briefs ─────────────────────────────────────────────────────────────────────

async fn list_briefs(
    State(state): State<AppState>,
    Query(params): Query<StoryParams>,
) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    let briefs = state
        .store
        .list_briefs(&workspace_id, params.limit.unwrap_or(30).clamp(1, MAX_LIMIT))
        .await?;
    Ok(Json(json!({ "briefs": briefs })))
}

async fn get_brief(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let brief = state
        .store
        .get_brief(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound("brief".into()))?;
    Ok(Json(serde_json::to_value(brief)?))
}

async fn generate_brief(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    if state.host.is_none() {
        return Err(ApiError::Unavailable(crate::host::no_host_message().into()));
    }
    let brief = service::generate_brief(&state, BriefTrigger::Manual, now_ms()).await?;
    Ok(Json(serde_json::to_value(brief)?))
}

// ── Settings ───────────────────────────────────────────────────────────────────

async fn get_settings(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    Ok(Json(serde_json::to_value(
        state.store.get_settings(&workspace_id).await?,
    )?))
}

async fn put_settings(
    State(state): State<AppState>,
    Json(body): Json<NewsSettings>,
) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    state.store.save_settings(&workspace_id, &body).await?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Value {
        serde_json::from_str(include_str!("../../manifest.json")).expect("valid JSON")
    }

    fn declared_routes() -> Vec<String> {
        manifest()["sidecars"][0]["http"]["routes"]
            .as_array()
            .expect("routes must be an array")
            .iter()
            .map(|r| r["path"].as_str().expect("a path").to_owned())
            .collect()
    }

    #[test]
    fn every_served_route_is_declared_in_the_manifest() {
        // Core's ext-proxy matches with an EXACT segment count, so an undeclared path
        // is a hard 404 that reads like a bug in this file rather than a missing line
        // in a JSON document three directories away.
        let declared = declared_routes();
        for route in SERVED_ROUTES {
            assert!(
                declared.iter().any(|d| d == route),
                "'{route}' is served but not declared in manifest.json"
            );
        }
    }

    #[test]
    fn every_declared_route_is_actually_served() {
        // The other direction, which is worse when it breaks: a workflow author reads
        // the manifest and binds to a route that 404s.
        for route in declared_routes() {
            assert!(
                SERVED_ROUTES.contains(&route.as_str()),
                "'{route}' is declared in manifest.json but nothing serves it"
            );
        }
    }

    #[test]
    fn the_manifest_port_mount_and_id_match_the_process_constants() {
        let manifest = manifest();
        assert_eq!(manifest["sidecars"][0]["port"], 8008);
        assert_eq!(manifest["sidecars"][0]["http"]["mount"], "/api/news");
        assert_eq!(manifest["id"], crate::state::PLUGIN_ID);
    }

    #[test]
    fn every_hook_event_id_is_namespaced_to_this_manifest() {
        let manifest = manifest();
        let id = manifest["id"].as_str().expect("an id");
        for event in manifest["contributes"]["hook_events"]
            .as_array()
            .expect("hook_events must be an array")
        {
            let event_id = event["id"].as_str().expect("an event id");
            assert!(
                event_id.starts_with(&format!("{id}#")),
                "'{event_id}' is not namespaced to '{id}'"
            );
        }
    }

    #[test]
    fn a_bad_topic_query_is_a_bad_request_carrying_its_column() {
        // The whole point of the query grammar reaching the API surface. A watch that
        // silently matches nothing is worse than one that will not save.
        let err = compile("chip AND").expect_err("must refuse");
        match err {
            ApiError::BadRequest(message) => {
                assert!(message.contains("column"), "{message}");
            }
            other => panic!("expected a bad request, got {other:?}"),
        }
    }

    #[test]
    fn a_good_topic_query_compiles_to_a_storable_ast() {
        let (ast, node) = compile("(chip OR wafer) AND NOT title:sport").expect("must compile");
        let back: query::Node = serde_json::from_value(ast).expect("round-trips");
        assert_eq!(back, node);
    }
}
