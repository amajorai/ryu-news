//! The MCP server: four tools, so an agent can ground an answer in YOUR feed.
//!
//! `search` is the one that matters. It queries the local, already-deduplicated,
//! already-clustered corpus **with no web call at all** — so an agent answering "what
//! happened with X this week" reads the sources you chose and vetted, rather than
//! running a fresh web search and trusting whatever comes back. That is a different
//! and much stronger guarantee than a search tool can offer, and it is the reason this
//! app ships an MCP server rather than just a UI.
//!
//! # Two things that silently break this
//!
//! - **stdout is the wire.** Every log line goes to stderr. One `println!` or a
//!   default-writer subscriber desynchronizes JSON-RPC framing and every later frame
//!   is discarded, with no error anywhere.
//! - **Tool names are registered BARE.** Core forms `{server}.{tool}` from the
//!   `mcp_servers` key, so registering `news.search` here yields `news.news.search`.

use std::io::Write as _;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, BufReader};

use crate::{
    models::{now_ms, ArticleQuery, BriefTrigger},
    query, service,
    state::AppState,
};

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Default and maximum results for `search`.
const DEFAULT_SEARCH_LIMIT: usize = 20;
const MAX_SEARCH_LIMIT: usize = 100;

fn tools() -> Value {
    json!([
        {
            "name": "search",
            "description": "Search the user's OWN news corpus — the sources they subscribed to, \
                already deduplicated and clustered into stories. This makes NO web request: it reads \
                what has already been fetched, so results are limited to the user's sources and are \
                the same ones they see in the app. Prefer this over a web search when the question is \
                about what the user follows. Accepts the same boolean query language as a saved topic: \
                AND / OR / NOT, \"quoted phrases\", and field scoping like title: or source:.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Boolean query. AND/OR/NOT, phrases, and title:/body:/source:/author:/url: scoping." },
                    "since_hours": { "type": "integer", "description": "Only articles from the last N hours (default 168, one week)." },
                    "limit": { "type": "integer", "description": "Maximum articles (default 20, max 100)." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "story",
            "description": "Fetch one story with EVERY article clustered into it, across every outlet \
                covering it. Use this when the user asks how coverage differs, who else reported \
                something, or wants the spread of framing rather than a single account.",
            "inputSchema": {
                "type": "object",
                "properties": { "story_id": { "type": "string" } },
                "required": ["story_id"]
            }
        },
        {
            "name": "brief",
            "description": "The most recent news brief: one entry per story from the last day, with \
                its source count. Read-only by default. Pass generate=true to write a fresh one, which \
                costs a model call — do that only when the user explicitly asks for a new brief.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "generate": { "type": "boolean", "description": "Write a new brief instead of returning the latest." }
                }
            }
        },
        {
            "name": "topics",
            "description": "List the user's saved topic watches, with each one's query and whether it \
                is enabled. Useful for answering what the user is tracking, and for checking whether a \
                subject they are asking about is already being watched.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

/// Serve the MCP protocol on stdin/stdout until the client closes the stream.
pub async fn serve(state: AppState) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(line) else {
            tracing::warn!("mcp: skipping a frame that was not JSON");
            continue;
        };
        let Some(id) = request.get("id").cloned() else {
            continue; // a notification
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));

        let response = match method {
            "initialize" => ok(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "news", "version": env!("CARGO_PKG_VERSION") }
                }),
            ),
            "ping" => ok(id, json!({})),
            "tools/list" => ok(id, json!({ "tools": tools() })),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match call(&state, &name, &args).await {
                    Ok(value) => ok(id, content(&value, false)),
                    // An `isError` RESULT, not a JSON-RPC error: a protocol error is
                    // never shown to the model, so it would retry into silence.
                    Err(err) => ok(id, content(&json!({ "error": err.to_string() }), true)),
                }
            }
            other => err(id, -32601, &format!("unknown method '{other}'")),
        };
        emit(&response);
    }
    Ok(())
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn content(value: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// Write one frame and FLUSH — stdout to a pipe is block-buffered, and without the
/// flush the client waits forever for a reply sitting in this process's buffer.
fn emit(frame: &Value) {
    let mut stdout = std::io::stdout().lock();
    if writeln!(stdout, "{frame}").is_ok() {
        let _ = stdout.flush();
    }
}

async fn call(state: &AppState, name: &str, args: &Value) -> Result<Value> {
    let now = now_ms();
    let workspaces = state.store.list_workspaces().await?;
    let workspace = workspaces
        .first()
        .ok_or_else(|| anyhow::anyhow!("there is no workspace yet"))?;

    match name {
        "search" => {
            let raw = args
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("query is required"))?;
            // Parsed with the SAME grammar a saved topic uses, so what an agent can
            // ask for and what a watch can fire on are exactly the same language.
            let node = query::parse(raw).map_err(|e| anyhow::anyhow!("{e}"))?;
            let since_hours = args
                .get("since_hours")
                .and_then(Value::as_i64)
                .unwrap_or(168)
                .clamp(1, 24 * 90);
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_SEARCH_LIMIT, |v| v as usize)
                .clamp(1, MAX_SEARCH_LIMIT);

            let articles = state
                .store
                .list_articles(
                    &workspace.id,
                    &ArticleQuery {
                        since: Some(now - since_hours * 3_600_000),
                        limit: Some(1000),
                        ..Default::default()
                    },
                )
                .await?;
            let hits: Vec<Value> = articles
                .iter()
                .filter(|article| {
                    node.matches(&query::Document::new(
                        &article.title,
                        article.content.as_deref().unwrap_or(""),
                        "",
                        article.author.as_deref().unwrap_or(""),
                        &article.url,
                    ))
                })
                .take(limit)
                .map(|article| {
                    json!({
                        "article_id": article.id,
                        "title": article.title,
                        "url": article.url,
                        "author": article.author,
                        "summary": article.summary,
                        "published_at": article.published_at,
                        "story_id": article.story_id,
                    })
                })
                .collect();
            Ok(json!({ "articles": hits, "searched": articles.len(), "web_request_made": false }))
        }
        "story" => {
            let story_id = args
                .get("story_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("story_id is required"))?;
            let story = state
                .store
                .get_story(story_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no such story"))?;
            let articles = state
                .store
                .list_articles(
                    &workspace.id,
                    &ArticleQuery {
                        story_id: Some(story_id.to_owned()),
                        limit: Some(100),
                        // Duplicates INCLUDED here on purpose: "who else ran this" is
                        // the question, and a syndicated copy is a real answer to it.
                        include_duplicates: true,
                        ..Default::default()
                    },
                )
                .await?;
            Ok(json!({ "story": story, "articles": articles }))
        }
        "brief" => {
            if args.get("generate").and_then(Value::as_bool) == Some(true) {
                let brief = service::generate_brief(state, BriefTrigger::Manual, now).await?;
                return Ok(serde_json::to_value(brief)?);
            }
            match state.store.latest_brief(&workspace.id).await? {
                Some(brief) => Ok(serde_json::to_value(brief)?),
                None => Ok(json!({
                    "brief": Value::Null,
                    "note": "no brief has been written yet — pass generate=true to write one"
                })),
            }
        }
        "topics" => {
            let topics = state.store.list_topics(&workspace.id).await?;
            Ok(json!({
                "topics": topics.iter().map(|t| json!({
                    "topic_id": t.id,
                    "name": t.name,
                    "query": t.query,
                    "enabled": t.enabled,
                })).collect::<Vec<_>>()
            }))
        }
        other => Err(anyhow::anyhow!("unknown tool '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tool_name_is_self_prefixed() {
        // Core forms `{server}.{tool}`, so a self-prefixed name here becomes
        // `news.news.search` and no caller can ever reach it.
        for tool in tools().as_array().expect("an array") {
            let name = tool["name"].as_str().expect("a name");
            assert!(!name.contains('.'), "'{name}' is self-prefixed");
            assert!(
                !name.starts_with("news"),
                "'{name}' repeats the server name"
            );
        }
    }

    #[test]
    fn every_tool_has_an_object_schema_and_a_real_description() {
        for tool in tools().as_array().expect("an array") {
            let name = tool["name"].as_str().expect("a name");
            let description = tool["description"].as_str().unwrap_or_default();
            assert!(description.len() > 40, "'{name}' description is too terse");
            assert_eq!(
                tool["inputSchema"]["type"], "object",
                "'{name}' must take an object"
            );
        }
    }

    #[test]
    fn the_search_description_states_that_it_makes_no_web_request() {
        // The whole value of this tool over a web search is that it reads the user's
        // OWN vetted corpus. A model that does not know that will not reach for it.
        let table = tools();
        let search = table
            .as_array()
            .expect("an array")
            .iter()
            .find(|t| t["name"] == "search")
            .expect("a search tool");
        let description = search["description"].as_str().unwrap_or_default();
        assert!(description.contains("NO web request"), "{description}");
    }

    #[test]
    fn the_tool_set_matches_what_the_manifest_advertises() {
        let table = tools();
        let mut names: Vec<&str> = table
            .as_array()
            .expect("an array")
            .iter()
            .map(|t| t["name"].as_str().expect("a name"))
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["brief", "search", "story", "topics"]);
    }

    #[test]
    fn a_tool_failure_is_a_result_not_a_protocol_error() {
        let frame = content(&json!({ "error": "no such story" }), true);
        assert_eq!(frame["isError"], true);
        assert!(frame["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("no such story"));
    }
}
