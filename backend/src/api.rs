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
//!
//! # …and in the OpenAPI document
//!
//! Core fetches `GET /openapi.json` from this sidecar on its first Healthy edge and
//! lowers every operation it finds into a searchable LLM tool — then keeps only the
//! ones the manifest ALSO declares. So a route with no `#[utoipa::path]` annotation
//! contributes no tool at all: nothing errors, an agent simply cannot reach it.
//!
//! This app also ships a stdio MCP server (`ryu-news mcp`), but that exposes four
//! READ verbs only — `search`, `story`, `brief`, `topics`. Everything that writes
//! (subscribing to a feed, saving an article, creating a topic watch) is reachable
//! solely through the derived tools, which is why the annotations below are not
//! redundant with it. Where the two do overlap, the summaries here say what the HTTP
//! operation gives that the MCP tool does not.
//!
//! The annotations carry the ABSOLUTE external path in `{param}` form while the router
//! registers paths RELATIVE to the mount in axum's `:param` form. The two forms differ
//! on purpose — Core nests this router at the mount, and a caller hits the absolute
//! path. The test normalises between them; do not "align" either side.

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

/// The document Core imports, served at `GET /openapi.json` by `main.rs`.
///
/// `components(schemas(...))` is what turns each `request_body = T` into a resolvable
/// `#/components/schemas/T`: without it the operation still carries a `$ref`, but the
/// target is missing and Core's `resolve_ref` yields nothing — a derived write tool
/// with zero visible arguments. utoipa 5 also auto-collects schemas reachable from the
/// annotated paths, so these rows are belt-and-braces; they are listed explicitly
/// anyway so the registration is greppable and cannot be lost to an attribute edit.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        create_source,
        create_topic,
        delete_source,
        delete_topic,
        export_opml,
        follow_story,
        generate_brief,
        get_article,
        get_brief,
        get_settings,
        get_source,
        get_story,
        get_topic,
        import_opml,
        list_articles,
        list_briefs,
        list_sources,
        list_stories,
        list_topics,
        mark_archived,
        mark_read,
        mark_saved,
        put_settings,
        refresh_source,
        topic_matches,
        update_source,
        update_topic,
    ),
    components(schemas(
        FollowBody,
        MarkBody,
        NewsSettings,
        OpmlBody,
        SourceBody,
        TopicBody,
    ))
)]
struct NewsApiDoc;

pub fn openapi() -> utoipa::openapi::OpenApi {
    <NewsApiDoc as utoipa::OpenApi>::openapi()
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

#[utoipa::path(
    get,
    path = "/api/news/sources",
    tag = "Wire",
    summary = "list the feeds the user subscribes to, with each one's fetch health.",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn list_sources(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    Ok(Json(json!({ "sources": state.store.list_sources(&workspace_id).await? })))
}

/// Request body for subscribing to a feed, and for editing one.
///
/// The field docs are not decoration: they are lifted verbatim into the OpenAPI schema
/// and become the argument descriptions a model reads when it decides what to send.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct SourceBody {
    /// Display name. Omit on create and the first fetch fills in the feed's own
    /// title; on update this REPLACES the name, so send the current one to keep it.
    title: Option<String>,
    /// The feed's URL. Must be `http`/`https`, and must be the FEED, not the site
    /// homepage. A URL already subscribed to is a 409, not a silent success.
    feed_url: String,
    /// The publication's homepage, for display. Optional.
    site_url: Option<String>,
    /// `rss`, `atom` or `json`. Omit — the first fetch detects it, and a wrong guess
    /// is overwritten anyway.
    kind: Option<String>,
    /// Whether to keep polling this feed. Update only; defaults to true.
    enabled: Option<bool>,
}

#[utoipa::path(
    post,
    path = "/api/news/sources",
    tag = "Wire",
    summary = "subscribe to a new feed.",
    request_body = SourceBody,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

#[utoipa::path(
    get,
    path = "/api/news/sources/{id}",
    tag = "Wire",
    summary = "read one subscribed feed and its fetch state.",
    params(("id" = String, Path, description = "Source id")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn get_source(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let source = state
        .store
        .get_source(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound("source".into()))?;
    Ok(Json(serde_json::to_value(source)?))
}

#[utoipa::path(
    patch,
    path = "/api/news/sources/{id}",
    tag = "Wire",
    summary = "rename a feed, or pause and resume polling it.",
    params(("id" = String, Path, description = "Source id")),
    // Every field is written, not merged: an omitted `title` clears the name and an
    // omitted `enabled` resumes polling.
    request_body = SourceBody,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

#[utoipa::path(
    delete,
    path = "/api/news/sources/{id}",
    tag = "Wire",
    summary = "unsubscribe from a feed.",
    params(("id" = String, Path, description = "Source id")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn delete_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_hit(state.store.delete_source(&id).await?, "source")
}

#[utoipa::path(
    post,
    path = "/api/news/sources/{id}/refresh",
    tag = "Wire",
    summary = "fetch one feed right now instead of waiting for the next poll; returns what was new.",
    params(("id" = String, Path, description = "Source id")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

#[utoipa::path(
    get,
    path = "/api/news/sources/opml",
    tag = "Wire",
    summary = "export every subscription as an OPML document.",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn export_opml(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    let sources = state.store.list_sources(&workspace_id).await?;
    Ok(Json(json!({ "opml": service::to_opml(&sources) })))
}

/// Request body for a bulk subscription import.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct OpmlBody {
    /// The OPML document, as exported by another reader. Feeds already subscribed to
    /// are counted as `skipped` rather than duplicated.
    opml: String,
}

#[utoipa::path(
    post,
    path = "/api/news/sources/opml",
    tag = "Wire",
    summary = "bulk-subscribe from an OPML document exported by another reader.",
    request_body = OpmlBody,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

#[utoipa::path(
    get,
    path = "/api/news/articles",
    tag = "Wire",
    summary = "the ranked feed: articles from the user's own subscriptions, each carrying why it ranks where it does.",
    params(
        ("story_id" = Option<String>, Query, description = "Only articles clustered into one story."),
        ("source_id" = Option<String>, Query, description = "Only articles from one subscribed feed."),
        ("unread" = Option<bool>, Query, description = "true = only unread, false = only read, omit = both."),
        ("saved" = Option<bool>, Query, description = "true = only saved, false = only unsaved, omit = both."),
        ("archived" = Option<bool>, Query, description = "true = only archived. Omit to exclude archived."),
        ("since_hours" = Option<i64>, Query, description = "Only articles published in the last N hours."),
        ("limit" = Option<usize>, Query, description = "Maximum articles. Default 200, clamped to 500."),
    ),
    responses((status = 200, description = "OK — each article carries a `rank` object explaining its position", body = serde_json::Value))
)]
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

#[utoipa::path(
    get,
    path = "/api/news/articles/{id}",
    tag = "Wire",
    summary = "read one article with its extracted text.",
    params(("id" = String, Path, description = "Article id")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn get_article(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let article = state
        .store
        .get_article(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound("article".into()))?;
    Ok(Json(serde_json::to_value(article)?))
}

/// Which way to flip an article's read / saved / archived flag.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct MarkBody {
    /// `true` sets the flag, `false` clears it. Defaults to `true`.
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

#[utoipa::path(
    post,
    path = "/api/news/articles/{id}/read",
    tag = "Wire",
    summary = "mark an article read, or unread.",
    params(("id" = String, Path, description = "Article id")),
    // NOT `Option<MarkBody>`, even though the handler takes `Option<Json<MarkBody>>`
    // and an absent body means `on: true`. utoipa 5 renders an optional request body
    // as `{"oneOf":[{"type":"null"},{"$ref":…}]}`, and Core's importer resolves only a
    // TOP-LEVEL `$ref` — a `oneOf` node passes through unresolved, has no `properties`,
    // and the derived tool is back to zero arguments. The cost is a body documented as
    // required that the handler in fact tolerates omitting: a far smaller lie than an
    // uncallable tool.
    request_body = MarkBody,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn mark_read(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<MarkBody>>,
) -> ApiResult<Json<Value>> {
    mark(&state, &id, ArticleMark::Read, body.map_or(true, |b| b.on)).await
}

#[utoipa::path(
    post,
    path = "/api/news/articles/{id}/save",
    tag = "Wire",
    summary = "save an article for later, or unsave it.",
    params(("id" = String, Path, description = "Article id")),
    request_body = MarkBody,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn mark_saved(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<MarkBody>>,
) -> ApiResult<Json<Value>> {
    mark(&state, &id, ArticleMark::Saved, body.map_or(true, |b| b.on)).await
}

#[utoipa::path(
    post,
    path = "/api/news/articles/{id}/archive",
    tag = "Wire",
    summary = "archive an article out of the feed, or restore it.",
    params(("id" = String, Path, description = "Article id")),
    request_body = MarkBody,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

#[utoipa::path(
    get,
    path = "/api/news/stories",
    tag = "Wire",
    summary = "list clustered stories — one entry per event, however many outlets covered it.",
    params(
        ("since_hours" = Option<i64>, Query, description = "Only stories with coverage in the last N hours."),
        ("limit" = Option<usize>, Query, description = "Maximum stories. Default 200, clamped to 500."),
    ),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

#[utoipa::path(
    get,
    path = "/api/news/stories/{id}",
    tag = "Wire",
    summary = "one story plus every article clustered into it, across all outlets — how coverage differs and who else ran it.",
    params(("id" = String, Path, description = "Story id")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

/// Whether to keep following a story as it develops.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct FollowBody {
    /// `true` follows the story so new coverage surfaces, `false` unfollows.
    /// Defaults to `true`.
    #[serde(default = "yes")]
    followed: bool,
}

#[utoipa::path(
    post,
    path = "/api/news/stories/{id}/follow",
    tag = "Wire",
    summary = "follow a developing story so new coverage surfaces, or unfollow it.",
    params(("id" = String, Path, description = "Story id")),
    // Plain, not `Option<FollowBody>` — see the note on `mark_read`.
    request_body = FollowBody,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn follow_story(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<FollowBody>>,
) -> ApiResult<Json<Value>> {
    let followed = body.map_or(true, |b| b.followed);
    require_hit(state.store.set_story_followed(&id, followed).await?, "story")
}

// ── Topics ─────────────────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/news/topics",
    tag = "Wire",
    summary = "list the user's saved topic watches, each with its query and whether it is on.",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn list_topics(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    Ok(Json(json!({ "topics": state.store.list_topics(&workspace_id).await? })))
}

/// Request body for saving a topic watch.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
struct TopicBody {
    /// What to call the watch, e.g. "EU chip policy". Must be unique; a clash is a
    /// 409.
    name: String,
    /// The boolean query that decides what matches: `AND` / `OR` / `NOT`, `"quoted
    /// phrases"`, parentheses, and field scoping like `title:`, `body:`, `source:`,
    /// `author:`, `url:`. A query that does not parse is refused with a 400 naming
    /// the column, rather than saved as a watch that silently matches nothing.
    query: String,
    /// Whether the watch fires burst alerts. Update only; defaults to true.
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

#[utoipa::path(
    post,
    path = "/api/news/topics",
    tag = "Wire",
    summary = "save a boolean topic watch that alerts when coverage of it bursts.",
    request_body = TopicBody,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

#[utoipa::path(
    get,
    path = "/api/news/topics/{id}",
    tag = "Wire",
    summary = "read one saved topic watch.",
    params(("id" = String, Path, description = "Topic id")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn get_topic(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let topic = state
        .store
        .get_topic(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound("topic".into()))?;
    Ok(Json(serde_json::to_value(topic)?))
}

#[utoipa::path(
    patch,
    path = "/api/news/topics/{id}",
    tag = "Wire",
    summary = "rewrite a topic watch's query or name, or turn it off.",
    params(("id" = String, Path, description = "Topic id")),
    request_body = TopicBody,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

#[utoipa::path(
    delete,
    path = "/api/news/topics/{id}",
    tag = "Wire",
    summary = "delete a saved topic watch.",
    params(("id" = String, Path, description = "Topic id")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn delete_topic(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_hit(state.store.delete_topic(&id).await?, "topic")
}

#[utoipa::path(
    get,
    path = "/api/news/topics/{id}/matches",
    tag = "Wire",
    summary = "the articles a saved topic watch matched, plus when it last burst.",
    params(
        ("id" = String, Path, description = "Topic id"),
        ("since_hours" = Option<i64>, Query, description = "Look back this many hours. Default 168 (one week)."),
        ("limit" = Option<usize>, Query, description = "Maximum articles. Default 100, clamped to 500."),
    ),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

#[utoipa::path(
    get,
    path = "/api/news/briefs",
    tag = "Wire",
    summary = "list past news briefs, newest first. Reads only; writes nothing and costs no model call.",
    params(
        ("limit" = Option<usize>, Query, description = "Maximum briefs. Default 30, clamped to 500."),
    ),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

#[utoipa::path(
    get,
    path = "/api/news/briefs/{id}",
    tag = "Wire",
    summary = "read one news brief in full.",
    params(("id" = String, Path, description = "Brief id")),
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn get_brief(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let brief = state
        .store
        .get_brief(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound("brief".into()))?;
    Ok(Json(serde_json::to_value(brief)?))
}

#[utoipa::path(
    post,
    path = "/api/news/briefs/generate",
    tag = "Wire",
    summary = "write a fresh news brief now. Costs a model call — read the latest brief instead unless a new one was asked for.",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn generate_brief(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    if state.host.is_none() {
        return Err(ApiError::Unavailable(crate::host::no_host_message().into()));
    }
    let brief = service::generate_brief(&state, BriefTrigger::Manual, now_ms()).await?;
    Ok(Json(serde_json::to_value(brief)?))
}

// ── Settings ───────────────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/news/settings",
    tag = "Wire",
    summary = "read the newsroom's polling, dedupe, clustering, ranking and burst knobs.",
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
async fn get_settings(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let workspace_id = workspace(&state).await?;
    Ok(Json(serde_json::to_value(
        state.store.get_settings(&workspace_id).await?,
    )?))
}

#[utoipa::path(
    put,
    path = "/api/news/settings",
    tag = "Wire",
    summary = "change the newsroom's polling, dedupe, clustering, ranking and burst knobs.",
    // Unlike the other bodies here this one is `#[serde(default)]`, so an omitted field
    // falls back to its default rather than erroring — which means a partial write
    // silently RESETS everything left out. Read `GET /settings` first and send it back
    // changed.
    request_body = NewsSettings,
    responses((status = 200, description = "OK", body = serde_json::Value))
)]
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

    /// The OpenAPI document as plain JSON, for the schema assertions below.
    fn doc_json() -> Value {
        serde_json::to_value(openapi()).expect("the document serializes")
    }

    /// A manifest route (relative to the mount, in axum's `:param` form) rewritten into
    /// the form the OpenAPI document uses (absolute, in `{param}` form).
    ///
    /// The two forms differ ON PURPOSE — the router registers paths relative to the
    /// mount because Core nests it there, while the `#[utoipa::path]` annotations carry
    /// the absolute EXTERNAL path a caller actually hits. Normalise here; do not
    /// "align" either side.
    fn doc_path_for(mount: &str, route: &str) -> String {
        let joined = if route == "/" {
            mount.to_owned()
        } else {
            format!("{mount}{route}")
        };
        joined
            .split('/')
            .map(|seg| match seg.strip_prefix(':') {
                Some(name) => format!("{{{name}}}"),
                None => seg.to_owned(),
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// The request-body schema node for one operation, or `None` if it documents no
    /// body.
    fn request_body_schema<'a>(doc: &'a Value, path: &str, method: &str) -> Option<&'a Value> {
        let escaped = path.replace('/', "~1");
        doc.pointer(&format!(
            "/paths/{escaped}/{method}/requestBody/content/application~1json/schema"
        ))
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
    fn openapi_doc_covers_the_served_routes() {
        // The document is not dead code: `main.rs` serves it and Core fetches it to
        // derive tools, so an empty one means this app contributes nothing.
        let doc = openapi();
        assert!(!doc.paths.paths.is_empty());
    }

    #[test]
    fn every_declared_route_appears_in_the_openapi_doc() {
        // The third direction, and the one that decides tool yield. Core's importer
        // keeps only the document operations the manifest ALSO declares, so a declared
        // route with no `#[utoipa::path]` annotation is a tool that silently never
        // exists — nothing errors, the agent simply cannot call it.
        //
        // NOTE this checks PATHS, not methods: it passes as soon as one operation
        // exists at `/api/news/topics/{id}`, so it cannot catch an unannotated `patch`
        // next to an annotated `get`. Green here is necessary, not sufficient — the
        // method-level check below is what closes that gap.
        let mount = manifest()["sidecars"][0]["http"]["mount"]
            .as_str()
            .expect("an http.mount")
            .to_owned();
        let doc = openapi();
        for route in declared_routes() {
            let expected = doc_path_for(&mount, &route);
            assert!(
                doc.paths.paths.contains_key(&expected),
                "'{route}' is declared in manifest.json but the OpenAPI document has no \
                 '{expected}' operation — Core derives no tool for it"
            );
        }
    }

    #[test]
    fn every_served_route_carries_an_operation_for_each_method_it_serves() {
        // What the path-level check above cannot see. The pairs are written out rather
        // than derived because axum's `Router` does not expose its method table either,
        // so this is the same duplication `SERVED_ROUTES` already accepts — and the
        // same payoff: a handler that loses its annotation fails here.
        let doc = doc_json();
        let methods: &[(&str, &[&str])] = &[
            ("/sources", &["get", "post"]),
            ("/sources/opml", &["get", "post"]),
            ("/sources/:id", &["get", "patch", "delete"]),
            ("/sources/:id/refresh", &["post"]),
            ("/articles", &["get"]),
            ("/articles/:id", &["get"]),
            ("/articles/:id/read", &["post"]),
            ("/articles/:id/save", &["post"]),
            ("/articles/:id/archive", &["post"]),
            ("/stories", &["get"]),
            ("/stories/:id", &["get"]),
            ("/stories/:id/follow", &["post"]),
            ("/topics", &["get", "post"]),
            ("/topics/:id", &["get", "patch", "delete"]),
            ("/topics/:id/matches", &["get"]),
            ("/briefs", &["get"]),
            ("/briefs/generate", &["post"]),
            ("/briefs/:id", &["get"]),
            ("/settings", &["get", "put"]),
        ];
        assert_eq!(
            methods.len(),
            SERVED_ROUTES.len(),
            "the method table must cover every served route"
        );
        for (route, verbs) in methods {
            let path = doc_path_for("/api/news", route);
            let escaped = path.replace('/', "~1");
            for verb in *verbs {
                assert!(
                    doc.pointer(&format!("/paths/{escaped}/{verb}")).is_some(),
                    "{verb} {path} is served but carries no #[utoipa::path] annotation"
                );
            }
        }
    }

    #[test]
    fn the_settings_body_exposes_the_dedupe_choices_rather_than_a_pointer() {
        // Core follows a `$ref` only at the TOP of a schema node, so an un-inlined
        // `dedupe` would reach the model as an opaque pointer — the tool would compile,
        // appear in the document, pass every path check above, and still be uncallable
        // for the one argument that is not a number.
        let doc = doc_json();
        let body = request_body_schema(&doc, "/api/news/settings", "put")
            .expect("put /api/news/settings documents a request body");
        assert_eq!(
            body["$ref"],
            Value::String("#/components/schemas/NewsSettings".to_owned()),
            "the body must point at a resolvable component: {body:#}"
        );
        let schema = doc
            .pointer("/components/schemas/NewsSettings")
            .expect("NewsSettings is registered in components");
        let rendered = schema.to_string();
        assert!(
            !rendered.contains("$ref"),
            "NewsSettings still carries an unresolvable $ref: {schema:#}"
        );
        for needle in ["relaxed", "standard", "aggressive"] {
            assert!(
                rendered.contains(needle),
                "the dedupe choices must be visible; '{needle}' is missing: {schema:#}"
            );
        }
    }

    #[test]
    fn the_optional_mark_bodies_are_documented_as_plain_types() {
        // `Option<MarkBody>` would render `{"oneOf":[{"type":"null"},{"$ref":…}]}`,
        // which Core cannot resolve — the read/save/archive tools would lose their one
        // argument. The plain form keeps `on` visible.
        let doc = doc_json();
        for path in [
            "/api/news/articles/{id}/read",
            "/api/news/articles/{id}/save",
            "/api/news/articles/{id}/archive",
        ] {
            let body =
                request_body_schema(&doc, path, "post").expect("the mark body is documented");
            assert_eq!(
                body["$ref"],
                Value::String("#/components/schemas/MarkBody".to_owned()),
                "post {path} must document a plain MarkBody, not a nullable wrapper: {body:#}"
            );
        }
        assert!(
            doc.pointer("/components/schemas/MarkBody/properties/on")
                .is_some(),
            "MarkBody must expose its `on` argument"
        );
    }

    #[test]
    fn body_less_routes_declare_no_request_body() {
        // These take a path parameter or nothing at all. Documenting a body would
        // invent an argument the handler ignores.
        let doc = doc_json();
        for (path, method) in [
            ("/api/news/sources/{id}/refresh", "post"),
            ("/api/news/briefs/generate", "post"),
        ] {
            assert!(
                request_body_schema(&doc, path, method).is_none(),
                "{method} {path} must document no request body"
            );
        }
    }

    #[test]
    fn body_field_docs_reach_the_schema_as_argument_descriptions() {
        // Field doc comments are lifted verbatim into `description`, which is the text
        // the model actually reads when deciding how to fill an argument. The topic
        // query is the one that most needs it: it is a small language, and a model
        // guessing at its syntax saves a watch that matches nothing.
        let doc = doc_json();
        let query = doc
            .pointer("/components/schemas/TopicBody/properties/query/description")
            .and_then(Value::as_str)
            .expect("the `query` argument is described");
        assert!(
            query.contains("AND") && query.contains("title:"),
            "the description must teach the grammar it accepts: {query}"
        );
    }

    #[test]
    fn the_openapi_document_is_not_a_declared_route() {
        // It is served at the SERVER ROOT, inside the bearer gate but off the mount.
        // Declaring it would expose this app's whole internal API surface through the
        // generic ext-proxy — and would fail the two direction tests above, since
        // nothing under the mount serves it.
        assert!(!declared_routes().iter().any(|r| r.contains("openapi")));
        assert!(!SERVED_ROUTES.iter().any(|r| r.contains("openapi")));
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
