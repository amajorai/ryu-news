# ryu-news

Wire for Ryu — a personal newsroom: RSS/Atom/JSON-Feed ingest with conditional GET and per-source backoff, two-layer dedupe (URL canonicalization plus a banded SimHash over content shingles), cross-outlet clustering so the unit you read is an event with n sources, burst detection and explainable ranking.

> **The public home of `ryu-news`.** Source, builds, and releases live here —
> binaries for every platform are attached to each release.
>
> This tree is generated from the Ryu monorepo, so commits pushed here
> directly are replaced on the next sync. **Pull requests are welcome** —
> open them here and they are ported into the monorepo, then flow back out.
> Ryu as a whole: https://github.com/amajorai/ryu

## Install

- Binary: `ryu-news` from the [Ryu releases](https://github.com/amajorai/ryu/releases).
- Crate: `cargo install ryu-news`.

## License

Apache-2.0 — see [LICENSE](./LICENSE).

---

# Wire (`@ryu/news`)

Your own newsroom. Wire pulls feeds and pages in on a schedule, collapses the same story
across every outlet covering it, writes you a brief from those clusters, and fires watches
on bursts it can explain. It is a reader **and** a monitor, because those are the same
pipeline with two front ends.

The ingest loop is a Rust sidecar the node owns, so the morning brief is built whether or
not the desktop app is open.

## What it does, concretely

- **Sources.** RSS, Atom and JSON Feed parsed natively, with conditional GET (`ETag` /
  `Last-Modified`), exponential backoff on failure, and a source marked `failing` in the UI
  after three consecutive misses rather than quietly going silent. Feed-less or truncated
  sources go through the `web.extract` capability. OPML in and out.
- **Dedupe, twice.** URLs are canonicalized (redirects followed, `utm_*` and friends
  stripped, query pairs sorted) and content is fingerprinted with a 64-bit SimHash over word
  shingles, banded for fast lookup. That is what catches syndicated wire copy running under
  a different headline on six different sites — the single biggest source of feed noise.
- **Stories, not articles.** Articles are clustered across outlets by shingle similarity,
  entity overlap and title overlap inside a rolling window. The unit you read is a story
  with *n* sources attached, so "eight outlets are covering this, and here is how their
  framing differs" is a thing you can see rather than infer.
- **A brief you can audit.** The scheduled digest writes two sentences per cluster with the
  source links, and the raw cluster stays inspectable underneath it. Cluster titling and
  brief prose are the *only* two places a model is involved.
- **Watches that fire for a reason.** A topic is a real boolean query — `AND` / `OR` /
  `NOT`, phrases, and field scoping like `title:` or `source:` — parsed to an AST and
  evaluated against tokenized text, not substring-matched. It refuses to save on a parse
  error, with the column, because a watch that silently matches nothing is worse than one
  that will not save.
- **Breaking, defined.** Burst detection compares a topic's hourly volume against a baseline
  built from *the same hour of day* over the trailing week, because news has a daily cycle
  and a flat baseline just fires every weekday morning. An alert carries its z-score and the
  articles that caused it.
- **Ranking you can interrogate.** Recency half-life × source count × topic match × unread.
  Every factor is displayable, so the feed can answer "why is this at the top".

## How it uses the rest of Ryu

- **Agents ground in *your* corpus.** The app ships an MCP server: `news__search` queries
  the local, vetted, already-deduplicated corpus with no web call at all, and `news__brief`,
  `news__story` and `news__topics` round it out. An agent answering "what happened with X
  this week" can read the feed you curated instead of running a fresh web search and
  trusting whatever comes back.
- **Ground a message in the news.** Turn on *Ground in news* in the composer and a
  `pre_user_turn` hook attaches the top matching recent items as context before the message
  is sent.
- **Events worth routing.** `brief.ready`, `topic.breaking` and `story.developing` are
  declared `hook_events`, so notifications, the Inbox and workflows can bind to them.
- **Extraction** is a capability request, not a dependency on a particular scraper.

## Architecture

`apps-store/news/` is a self-contained satellite. `backend/` is the `ryu-news` sidecar
binary (axum + rusqlite, no lib target, no dependency on `apps/core`), reached through the
generic ext-proxy at `/api/news/*`. `ui/` is the sandboxed companion
(`vite-plugin-singlefile`, CSP `connect-src 'none'`), which talks to its own sidecar through
one generic forwarder rather than a verb per endpoint. The only thing that lands in Core is
the registration.

## Privacy

Sources, articles, clusters and read state live in the node's own SQLite database. Fetches
go to the sources you added and nowhere else — there is no aggregator in the middle, no
account, and no reading history leaving the machine. The clustering, dedupe, burst and
ranking math never calls anything.
