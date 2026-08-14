//! Feed parsing: a hand-written markup scanner over RSS 2.0 / RSS 1.0 / Atom 1.0,
//! and JSON Feed through `serde_json`.
//!
//! # Why by hand
//!
//! `feed-rs`, `quick-xml`, `rss` and `atom_syndication` are none of them in the root
//! `Cargo.lock`, and this app may not put them there — several jobs share that
//! lockfile (see the dependency note in `Cargo.toml`). So the scanner below is the
//! whole XML layer: tags, text, CDATA, comments, processing instructions, numeric and
//! named character references, and namespace prefixes dropped rather than resolved.
//!
//! # It parses hostile bytes, so it never panics and never recurses
//!
//! Every source is a server nobody here controls, and half of the feeds in the wild
//! are invalid in some way. The rules this file holds to:
//!
//! - **No recursion anywhere.** The scanner loops; the walk keeps an explicit stack
//!   with a [`MAX_XML_DEPTH`] cap. A recursive walk over 50 000 nested tags is a
//!   stack overflow, which aborts the process rather than unwinding — it could not
//!   even be tested for.
//! - **No slicing that can panic.** Every skip-ahead terminates at end-of-input.
//! - **Malformed input is an error, not a panic** ([`FeedError`]), and the caller
//!   turns that into `consecutive_failures += 1` plus the backoff in
//!   [`crate::models::backoff_hours`]. A feed that was *truncated* mid-download is
//!   the one exception: the items that did arrive are returned, because losing forty
//!   good items to one cut connection is a worse answer than a partial read.
//! - **`now` is a parameter.** An item with no usable date is stamped with the `now`
//!   the caller passed, so a replay of the same bytes produces the same articles.
//!
//! # The markup layer lives here and `extract` shares it
//!
//! [`Scanner`] and [`decode_entities`] are `pub(crate)` and [`crate::extract`] uses
//! them. An XML pull-tokenizer and an HTML one differ only in what the *walker* does
//! with the events — void elements, block-level line breaks, dropped subtrees are all
//! walker concerns. A second scanner over there would be the same class of
//! duplication [`crate::text`] exists to prevent, and the two copies would disagree
//! about CDATA the first time either was touched.

use crate::extract::html_to_text;
use crate::models::SourceKind;

/// Hard ceiling on element nesting before the document is called malformed.
pub const MAX_XML_DEPTH: usize = 64;

/// Ceiling on items taken from one feed document. Bounds the memory a hostile or
/// broken source can make this process allocate in one poll; matches
/// [`crate::models::MAX_LIMIT`], which is the most any list route would show anyway.
pub const MAX_ITEMS_PER_FEED: usize = 500;

/// Longest character reference [`decode_entities`] looks ahead for, in bytes.
/// `&thetasym;` is 9; an `&` followed by 200 characters and then a `;` is prose.
pub const MAX_ENTITY_LEN: usize = 12;

// ── The markup layer (shared with `crate::extract`) ────────────────────────────

/// The named references worth carrying.
///
/// Not the full HTML5 table (2231 entries, most of them mathematical): these are the
/// ones that appear in headlines and article bodies. An unrecognized reference is
/// left EXACTLY as written rather than dropped, so `Q&A` survives intact and a
/// `&hearts;` shows up as itself instead of silently vanishing mid-sentence.
///
/// Sorted; looked up by binary search, and a test asserts the sort.
const NAMED_ENTITIES: &[(&str, &str)] = &[
    ("AMP", "&"),
    ("GT", ">"),
    ("LT", "<"),
    ("QUOT", "\""),
    ("amp", "&"),
    ("apos", "'"),
    ("bull", "\u{2022}"),
    ("copy", "\u{a9}"),
    ("deg", "\u{b0}"),
    ("euro", "\u{20ac}"),
    ("gt", ">"),
    ("hellip", "\u{2026}"),
    ("laquo", "\u{ab}"),
    ("ldquo", "\u{201c}"),
    ("lsquo", "\u{2018}"),
    ("lt", "<"),
    ("mdash", "\u{2014}"),
    ("middot", "\u{b7}"),
    ("nbsp", "\u{a0}"),
    ("ndash", "\u{2013}"),
    ("pound", "\u{a3}"),
    ("quot", "\""),
    ("raquo", "\u{bb}"),
    ("rdquo", "\u{201d}"),
    ("reg", "\u{ae}"),
    ("rsquo", "\u{2019}"),
    ("sbquo", "\u{201a}"),
    ("times", "\u{d7}"),
    ("trade", "\u{2122}"),
    ("yen", "\u{a5}"),
];

/// Decode one reference body (`amp`, `#38`, `#x26`). `None` leaves it untouched.
fn decode_one(body: &str) -> Option<String> {
    if let Some(digits) = body.strip_prefix('#') {
        let code = if let Some(hex) = digits.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            digits.parse::<u32>().ok()?
        };
        // `char::from_u32` rejects surrogates and anything past U+10FFFF, which is
        // the whole validation this needs.
        return char::from_u32(code).map(String::from);
    }
    NAMED_ENTITIES
        .binary_search_by(|(name, _)| (*name).cmp(body))
        .ok()
        .map(|idx| NAMED_ENTITIES[idx].1.to_string())
}

/// Decode character references in ONE pass.
///
/// The single pass is the point. A decoder that loops until the output stops changing
/// turns `&amp;amp;…amp;lt;` into an amplification bomb, and a recursive one turns it
/// into a stack overflow. Here, decoded text is appended and never looked at again,
/// so `&amp;lt;` decodes to the literal four characters `&lt;` — which is exactly
/// what it means.
pub fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        let Some(ch) = rest.chars().next() else { break };
        if ch != '&' {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let after = &rest[1..];
        let semi = after
            .as_bytes()
            .iter()
            .take(MAX_ENTITY_LEN)
            .position(|b| *b == b';');
        match semi.and_then(|end| decode_one(&after[..end]).map(|d| (end, d))) {
            Some((end, decoded)) => {
                out.push_str(&decoded);
                i += 1 + end + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

/// One thing the scanner found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token<'a> {
    Start {
        /// Lowercased local name — see [`local_name`].
        name: String,
        /// The raw attribute text, for [`attr`].
        attrs: &'a str,
        self_closing: bool,
    },
    End {
        name: String,
    },
    /// Text that still carries character references.
    Text(&'a str),
    /// A CDATA section. Kept separate from [`Token::Text`] because its content is
    /// literal: `<![CDATA[AT&amp;T]]>` is the eight characters `AT&amp;T`, and
    /// decoding it at this layer would silently apply an extra round.
    CData(&'a str),
}

/// A forward-only markup scanner. No tree and no per-node allocation: the walkers
/// keep whatever state they need on their own explicit stacks.
pub(crate) struct Scanner<'a> {
    src: &'a str,
    pos: usize,
    /// Set when a tag was left unterminated at end of input — the signature of a cut
    /// connection. Callers use it to tell "truncated download" from "not a feed".
    pub(crate) truncated: bool,
}

impl<'a> Scanner<'a> {
    pub(crate) fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            truncated: false,
        }
    }

    /// Position of `needle` at or after `from`. Every skip goes through here so that
    /// "not found" means "consume the rest" rather than an unbounded loop.
    fn find_from(&self, from: usize, needle: &str) -> Option<usize> {
        self.src.get(from..)?.find(needle).map(|idx| from + idx)
    }

    /// End of the tag starting at `from`, honouring quoted attribute values so a `>`
    /// inside `alt="a > b"` does not truncate the tag.
    fn tag_end(&self, from: usize) -> Option<usize> {
        let bytes = self.src.as_bytes();
        let mut i = from;
        let mut quote: Option<u8> = None;
        while i < bytes.len() {
            let b = bytes[i];
            match quote {
                Some(q) if b == q => quote = None,
                Some(_) => {}
                None if b == b'"' || b == b'\'' => quote = Some(b),
                None if b == b'>' => return Some(i),
                None => {}
            }
            i += 1;
        }
        None
    }

    /// Text from `self.pos` up to the next `<`, advancing past it.
    fn text_until_tag(&mut self) -> Token<'a> {
        let end = self.find_from(self.pos + 1, "<").unwrap_or(self.src.len());
        let text = &self.src[self.pos..end];
        self.pos = end;
        Token::Text(text)
    }

    /// The next token, or `None` at end of input.
    ///
    /// A `loop`, not a recursive call on the skip paths: a document with 50 000
    /// comments would otherwise recurse 50 000 deep for tokens nobody wanted.
    pub(crate) fn next_token(&mut self) -> Option<Token<'a>> {
        loop {
            if self.pos >= self.src.len() {
                return None;
            }
            if self.src.as_bytes()[self.pos] != b'<' {
                return Some(self.text_until_tag());
            }

            let rest = &self.src[self.pos..];
            if rest.starts_with("<!--") {
                // Unterminated comment: everything after it is comment. Terminating
                // at end-of-input rather than erroring keeps a half-read page usable.
                self.pos = self
                    .find_from(self.pos + 4, "-->")
                    .map_or(self.src.len(), |i| i + 3);
                continue;
            }
            if rest.starts_with("<![CDATA[") {
                let start = self.pos + 9;
                let (text_end, next) = match self.find_from(start, "]]>") {
                    Some(i) => (i, i + 3),
                    None => (self.src.len(), self.src.len()),
                };
                let text = &self.src[start..text_end];
                self.pos = next;
                return Some(Token::CData(text));
            }
            if rest.starts_with("<!") || rest.starts_with("<?") {
                self.pos = self.tag_end(self.pos).map_or(self.src.len(), |i| i + 1);
                continue;
            }

            let is_close = rest.starts_with("</");
            let name_start = self.pos + if is_close { 2 } else { 1 };
            let first = self.src[name_start..].chars().next();
            // A `<` that does not start a tag is text — `5 < 6` in a headline is
            // common, and it must not eat the rest of the document.
            if !first.is_some_and(|c| c.is_ascii_alphabetic() || c == '_') {
                return Some(self.text_until_tag());
            }

            let Some(gt) = self.tag_end(name_start) else {
                // A tag left open at end of input: the remainder is markup, not text.
                self.truncated = true;
                self.pos = self.src.len();
                return None;
            };
            let inner = &self.src[name_start..gt];
            let self_closing = inner.ends_with('/');
            let inner = inner.strip_suffix('/').unwrap_or(inner);
            let name_end = inner
                .find(|c: char| c.is_whitespace())
                .unwrap_or(inner.len());
            let name = local_name(&inner[..name_end]);
            let attrs = inner[name_end..].trim();
            self.pos = gt + 1;
            return Some(if is_close {
                Token::End { name }
            } else {
                Token::Start {
                    name,
                    attrs,
                    self_closing,
                }
            });
        }
    }
}

/// Lowercased local name: `content:encoded` → `encoded`, `DIV` → `div`.
///
/// Namespace prefixes are DROPPED rather than resolved, which is the "loose namespace
/// handling" a real-world feed needs: a document that binds Atom to `a:` instead of
/// the default namespace is otherwise unreadable, and the set of prefixes feeds
/// actually use is not one anybody can enumerate. The cost is that
/// `dc:title` and `title` become the same element, which for these formats is what a
/// reader wants anyway.
pub(crate) fn local_name(raw: &str) -> String {
    let local = raw.rsplit(':').next().unwrap_or(raw);
    local.trim().to_ascii_lowercase()
}

/// Read one attribute out of a raw attribute string. Case-insensitive on the name,
/// tolerant of single quotes, double quotes and no quotes at all.
pub(crate) fn attr<'a>(attrs: &'a str, want: &str) -> Option<&'a str> {
    let bytes = attrs.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && !(bytes[i] as char).is_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        if name_start == i {
            i += 1;
            continue;
        }
        let name = &attrs[name_start..i];
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            // A valueless attribute (`hidden`). Not something any caller here wants,
            // but it must not desynchronize the scan of the ones that follow.
            if name.eq_ignore_ascii_case(want) {
                return Some("");
            }
            continue;
        }
        i += 1;
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let value = if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
            let quote = bytes[i];
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let v = &attrs[start..i.min(attrs.len())];
            i += 1;
            v
        } else {
            let start = i;
            while i < bytes.len() && !(bytes[i] as char).is_whitespace() {
                i += 1;
            }
            &attrs[start..i]
        };
        if name.eq_ignore_ascii_case(want) {
            return Some(value);
        }
    }
    None
}

// ── Errors ─────────────────────────────────────────────────────────────────────

/// Why a feed document could not be read.
///
/// The caller maps every variant onto the same source-health outcome —
/// `consecutive_failures += 1`, the backoff from
/// [`crate::models::backoff_hours`], `last_error` set to this message — so the
/// variants exist for the human reading the source list, not for control flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedError {
    /// The response body was empty or all whitespace. Usually a server error page
    /// with a 200 status.
    Empty,
    /// Not a feed at all: an HTML page, a login redirect, a JSON object with no
    /// `items`. The single most common failure when a user pastes a site URL rather
    /// than its feed URL.
    UnknownFormat,
    /// A feed, but structurally broken beyond what the scanner tolerates.
    Malformed(String),
    /// JSON that would not parse.
    Json(String),
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "the feed response was empty"),
            Self::UnknownFormat => write!(
                f,
                "this does not look like an RSS, Atom or JSON feed — check the feed URL"
            ),
            Self::Malformed(why) => write!(f, "the feed could not be parsed: {why}"),
            Self::Json(why) => write!(f, "the JSON feed could not be parsed: {why}"),
        }
    }
}

impl std::error::Error for FeedError {}

// ── Parsed shapes ──────────────────────────────────────────────────────────────

/// One item, as the document described it. Everything is already plain text:
/// character references decoded, embedded HTML flattened by [`crate::extract`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedItem {
    /// The feed's own identifier (`<guid>` / Atom `<id>` / JSON Feed `id`). Kept
    /// because a source that rewrites its URLs would otherwise re-import its history.
    pub guid: Option<String>,
    /// The RAW url as published — never canonicalized here. Non-optional: an item
    /// with nowhere to click is dropped rather than stored.
    pub url: String,
    pub title: String,
    pub author: Option<String>,
    pub summary: Option<String>,
    pub content: Option<String>,
    /// Epoch millis. Defaulted to the `now` passed to [`parse_feed`] when the item
    /// carries no usable date — undated items are common and dropping them would
    /// lose real articles, but leaving the field optional would push the same
    /// decision onto every caller.
    pub published_at: i64,
}

/// A whole feed document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFeed {
    pub title: Option<String>,
    /// The human site the feed belongs to, for the UI's link-out.
    pub site_url: Option<String>,
    /// Which parser read it. Stored on the source so the next poll does not re-sniff
    /// — plenty of servers answer `application/xml` for an Atom document.
    pub kind: SourceKind,
    pub items: Vec<FeedItem>,
    /// The document ended mid-tag and `items` is what arrived before the cut.
    pub truncated: bool,
}

/// Parse a feed document of any supported format.
///
/// `now` is used only to stamp items that carry no usable date; nothing else in here
/// reads a clock, so the same bytes plus the same `now` always produce the same
/// items.
pub fn parse_feed(body: &str, now: i64) -> Result<ParsedFeed, FeedError> {
    // A BOM is common on feeds produced by Windows tooling and would otherwise make
    // the first byte test fail.
    let trimmed = body.trim_start_matches('\u{feff}').trim_start();
    if trimmed.is_empty() {
        return Err(FeedError::Empty);
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return parse_json_feed(trimmed, now);
    }
    parse_xml_feed(trimmed, now)
}

// ── XML (RSS 2.0 / RSS 1.0 / Atom 1.0) ─────────────────────────────────────────

/// Which item-level element the scanner is currently inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Link,
    Guid,
    Summary,
    Content,
    Author,
    Published,
    Updated,
}

/// Map a local element name onto the field it fills.
///
/// Deliberately format-agnostic: RSS's `description` and Atom's `summary` mean the
/// same thing, `content:encoded` arrives as the local name `encoded`, and `dc:date`
/// as `date`. Handling them by local name is what lets one walk read all three
/// dialects instead of three walks disagreeing about which one a document is.
fn field_for(name: &str) -> Option<Field> {
    Some(match name {
        "title" => Field::Title,
        "link" => Field::Link,
        "guid" | "id" => Field::Guid,
        "description" | "summary" => Field::Summary,
        "encoded" | "content" => Field::Content,
        "author" | "creator" => Field::Author,
        "pubdate" | "published" => Field::Published,
        "updated" | "date" | "modified" => Field::Updated,
        _ => return None,
    })
}

#[derive(Debug, Default)]
struct ItemBuilder {
    title: String,
    link: String,
    guid: String,
    summary: String,
    content: String,
    author: String,
    published: String,
    updated: String,
}

impl ItemBuilder {
    /// First non-empty value wins for every field.
    ///
    /// Feeds repeat elements constantly — two `<link>`s, a `<title>` inside a nested
    /// `<media:group>`, an Atom `<author>` in the entry AND in the feed. Last-wins
    /// would make the item's title depend on how deep the document nests, which is
    /// not a property anyone can reason about.
    fn set(&mut self, field: Field, value: String) {
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        let slot = match field {
            Field::Title => &mut self.title,
            Field::Link => &mut self.link,
            Field::Guid => &mut self.guid,
            Field::Summary => &mut self.summary,
            Field::Content => &mut self.content,
            Field::Author => &mut self.author,
            Field::Published => &mut self.published,
            Field::Updated => &mut self.updated,
        };
        if slot.is_empty() {
            *slot = value.to_string();
        }
    }

    /// Turn the collected text into an item, or drop it.
    ///
    /// An item with no URL is DROPPED: [`crate::models::Article::url`] is what the
    /// user clicks, `articles(workspace_id, canonical_url)` is a real UNIQUE index,
    /// and a row with an empty URL would collide with every other undated, unlinked
    /// item in the workspace.
    fn finish(self, now: i64) -> Option<FeedItem> {
        let url = if !self.link.is_empty() {
            self.link
        } else if self.guid.starts_with("http://") || self.guid.starts_with("https://") {
            // `<guid isPermaLink="true">` is a URL, and plenty of generators emit the
            // permalink there and nowhere else.
            self.guid.clone()
        } else {
            return None;
        };
        let title = if self.title.is_empty() {
            url.clone()
        } else {
            self.title
        };
        Some(FeedItem {
            guid: (!self.guid.is_empty()).then_some(self.guid),
            url,
            title,
            author: (!self.author.is_empty()).then_some(self.author),
            summary: (!self.summary.is_empty()).then_some(self.summary),
            content: (!self.content.is_empty()).then_some(self.content),
            published_at: parse_date(&self.published)
                .or_else(|| parse_date(&self.updated))
                .unwrap_or(now),
        })
    }
}

/// Values that arrive as HTML get flattened; the rest are taken verbatim.
fn field_value(field: Field, raw: &str) -> String {
    match field {
        Field::Title | Field::Summary | Field::Content => html_to_text(raw),
        _ => raw.trim().to_string(),
    }
}

fn parse_xml_feed(body: &str, now: i64) -> Result<ParsedFeed, FeedError> {
    let mut scanner = Scanner::new(body);
    let mut stack: Vec<String> = Vec::new();
    let mut kind: Option<SourceKind> = None;
    let mut items: Vec<FeedItem> = Vec::new();
    let mut feed_title = String::new();
    let mut site_url = String::new();
    let mut item: Option<ItemBuilder> = None;
    let mut item_depth = 0usize;
    // (depth the field element sits at, which field it fills).
    let mut field: Option<(usize, Field)> = None;
    let mut buf = String::new();

    while let Some(token) = scanner.next_token() {
        match token {
            Token::Text(raw) => {
                if field.is_some() {
                    buf.push_str(&decode_entities(raw));
                }
            }
            Token::CData(raw) => {
                if field.is_some() {
                    // Literal by definition — see [`Token::CData`].
                    buf.push_str(raw);
                }
            }
            Token::Start {
                name,
                attrs,
                self_closing,
            } => {
                if kind.is_none() {
                    kind = Some(match name.as_str() {
                        // RSS 1.0 is `<rdf:RDF>`; the local name is `rdf`. Its items
                        // are `<item>` with the same child elements, so one walk
                        // reads both.
                        "rss" | "rdf" => SourceKind::Rss,
                        "feed" => SourceKind::Atom,
                        _ => return Err(FeedError::UnknownFormat),
                    });
                    if !self_closing {
                        stack.push(name);
                    }
                    continue;
                }
                if stack.len() >= MAX_XML_DEPTH {
                    return Err(FeedError::Malformed(format!(
                        "nesting deeper than {MAX_XML_DEPTH} elements"
                    )));
                }

                if item.is_none() && (name == "item" || name == "entry") {
                    item = Some(ItemBuilder::default());
                    item_depth = stack.len();
                    field = None;
                    buf.clear();
                    if !self_closing {
                        stack.push(name);
                    }
                    continue;
                }

                if field.is_none() {
                    // Atom's `<link>` carries its URL in an attribute and has no text,
                    // so it is handled here rather than as a text field. `rel="self"`
                    // points at the feed document, `rel="enclosure"` at a podcast
                    // audio file — neither is the article.
                    if name == "link" {
                        if let Some(href) = attr(attrs, "href") {
                            let rel = attr(attrs, "rel").unwrap_or("alternate");
                            if rel.eq_ignore_ascii_case("alternate") {
                                let href = decode_entities(href).trim().to_string();
                                match item.as_mut() {
                                    Some(builder) => builder.set(Field::Link, href),
                                    None if site_url.is_empty() => site_url = href,
                                    None => {}
                                }
                            }
                            if !self_closing {
                                stack.push(name);
                            }
                            continue;
                        }
                    }
                    let wanted = match (&item, field_for(&name)) {
                        (Some(_), Some(f)) => Some(f),
                        // At feed level only the title and the RSS `<link>` matter,
                        // and only before an item has been seen — a `<title>` after
                        // the first item is inside something else.
                        (None, Some(f @ (Field::Title | Field::Link))) => Some(f),
                        _ => None,
                    };
                    if let Some(f) = wanted {
                        if !self_closing {
                            field = Some((stack.len(), f));
                            buf.clear();
                        }
                    }
                }

                if !self_closing {
                    stack.push(name);
                }
            }
            Token::End { name } => {
                // Pop to the matching open element, closing anything left dangling in
                // between. An end tag with no opener is ignored: unbalanced markup is
                // common enough that erroring on it would reject working feeds.
                let Some(idx) = stack.iter().rposition(|open| *open == name) else {
                    continue;
                };
                stack.truncate(idx);

                if let Some((depth, f)) = field {
                    if stack.len() <= depth {
                        let value = field_value(f, &buf);
                        match item.as_mut() {
                            Some(builder) => builder.set(f, value),
                            None => match f {
                                Field::Title if feed_title.is_empty() => feed_title = value,
                                Field::Link if site_url.is_empty() => site_url = value,
                                _ => {}
                            },
                        }
                        field = None;
                        buf.clear();
                    }
                }

                if item.is_some() && stack.len() <= item_depth {
                    if let Some(finished) = item.take().and_then(|b| b.finish(now)) {
                        items.push(finished);
                        if items.len() >= MAX_ITEMS_PER_FEED {
                            break;
                        }
                    }
                }
            }
        }
    }

    // A document cut mid-item still yields the item, provided it got far enough to
    // have a URL. `finish` is what decides that.
    if let Some(finished) = item.take().and_then(|b| b.finish(now)) {
        items.push(finished);
    }

    let Some(kind) = kind else {
        return Err(FeedError::UnknownFormat);
    };

    // Two independent ways a download can end early, and the scanner only sees one
    // of them. `scanner.truncated` fires when the bytes stop in the middle of a tag
    // (`<link hre`). But a connection is just as likely to be cut in the middle of
    // TEXT (`<link>https://exam`), which is perfectly well-formed as far as the
    // tokenizer is concerned — it simply runs out of input with elements still open.
    //
    // A non-empty element stack at end of input is what actually distinguishes the
    // two cases: a complete document closes everything it opened. Without this, a
    // feed cut mid-text is reported as clean, the caller trusts it as the source's
    // full contents, and every item after the cut looks deliberately deleted.
    let truncated = scanner.truncated || !stack.is_empty();

    if truncated && items.is_empty() {
        return Err(FeedError::Malformed(
            "the document ended before any item was readable".into(),
        ));
    }
    Ok(ParsedFeed {
        title: (!feed_title.is_empty()).then_some(feed_title),
        site_url: (!site_url.is_empty()).then_some(site_url),
        kind,
        items,
        truncated,
    })
}

// ── JSON Feed 1.1 ──────────────────────────────────────────────────────────────

/// A string field, whatever the JSON type. JSON Feed's `id` is specified as a string
/// and emitted as a number by a good share of generators.
fn json_str(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn json_author(item: &serde_json::Value) -> Option<String> {
    // 1.1 replaced `author` with `authors`; both are in the wild, and either may be
    // an object or a bare string.
    if let Some(first) = item.get("authors").and_then(|a| a.as_array()).and_then(|a| a.first()) {
        if let Some(name) = json_str(first.get("name")).or_else(|| json_str(Some(first))) {
            return Some(name);
        }
    }
    let author = item.get("author")?;
    json_str(author.get("name")).or_else(|| json_str(Some(author)))
}

fn parse_json_feed(body: &str, now: i64) -> Result<ParsedFeed, FeedError> {
    let doc: serde_json::Value =
        serde_json::from_str(body).map_err(|e| FeedError::Json(e.to_string()))?;
    let raw_items = doc
        .get("items")
        .and_then(|i| i.as_array())
        .ok_or(FeedError::UnknownFormat)?;

    let mut items = Vec::new();
    for raw in raw_items.iter().take(MAX_ITEMS_PER_FEED) {
        let Some(url) = json_str(raw.get("url")).or_else(|| json_str(raw.get("external_url")))
        else {
            // Same rule as the XML side: no URL, no article.
            continue;
        };
        let guid = json_str(raw.get("id"));
        let title = json_str(raw.get("title"))
            .map(|t| html_to_text(&t))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| url.clone());
        let content = json_str(raw.get("content_text"))
            .or_else(|| json_str(raw.get("content_html")).map(|h| html_to_text(&h)))
            .filter(|c| !c.is_empty());
        let summary = json_str(raw.get("summary"))
            .map(|s| html_to_text(&s))
            .filter(|s| !s.is_empty());
        let published_at = json_str(raw.get("date_published"))
            .and_then(|d| parse_date(&d))
            .or_else(|| json_str(raw.get("date_modified")).and_then(|d| parse_date(&d)))
            .unwrap_or(now);
        items.push(FeedItem {
            guid,
            url,
            title,
            author: json_author(raw),
            summary,
            content,
            published_at,
        });
    }

    Ok(ParsedFeed {
        title: json_str(doc.get("title")).filter(|t| !t.is_empty()),
        site_url: json_str(doc.get("home_page_url")).filter(|u| !u.is_empty()),
        kind: SourceKind::JsonFeed,
        items,
        truncated: false,
    })
}

// ── Dates ──────────────────────────────────────────────────────────────────────

/// Formats accepted after RFC 2822 and RFC 3339 have both failed. All are read as
/// UTC, because a feed that omits the offset has already thrown that information
/// away and guessing the node's zone would make the same bytes parse differently on
/// two machines.
const NAIVE_DATE_FORMATS: &[&str] = &[
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%dT%H:%M:%S",
    "%Y/%m/%d %H:%M:%S",
    "%d %b %Y %H:%M:%S",
];

/// Parse a feed date to epoch millis.
///
/// RSS says RFC 2822 (`Tue, 10 Aug 2026 09:12:00 GMT`), Atom says RFC 3339
/// (`2026-08-10T09:12:00Z`), and JSON Feed says RFC 3339. Real feeds emit all of
/// those plus several things that are neither, so the ladder ends in a small list of
/// naive formats rather than in a failure — an item with an unparseable date is
/// stamped `now` by the caller, which is a better answer than dropping it.
pub fn parse_date(raw: &str) -> Option<i64> {
    let mut text = raw.trim();
    if text.is_empty() {
        return None;
    }
    // RFC 2822 allows a trailing comment (`+0000 (UTC)`) that chrono rejects.
    if let Some(open) = text.rfind('(') {
        if text.ends_with(')') {
            text = text[..open].trim();
        }
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(text) {
        return Some(dt.timestamp_millis());
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(dt.timestamp_millis());
    }
    for format in NAIVE_DATE_FORMATS {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Some(naive.and_utc().timestamp_millis());
        }
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed instant so `now`-defaulting is visible in assertions:
    /// 2026-08-10T09:12:00Z.
    const NOW: i64 = 1_786_353_120_000;

    #[test]
    fn entity_table_is_sorted_for_the_binary_search() {
        let names: Vec<&str> = NAMED_ENTITIES.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn entity_decoding_table() {
        let cases: &[(&str, &str, &str)] = &[
            ("named", "AT&amp;T", "AT&T"),
            ("angle brackets", "5 &lt; 6 &gt; 4", "5 < 6 > 4"),
            ("decimal numeric", "caf&#233;", "café"),
            ("hex numeric, either case", "caf&#xE9; caf&#Xe9;", "café café"),
            ("uppercase named form", "a &AMP; b", "a & b"),
            ("unknown entity is left alone", "Q&A and &hearts;", "Q&A and &hearts;"),
            ("a bare ampersand survives", "Tom & Jerry", "Tom & Jerry"),
            ("no semicolon in the window", "&averyverylongthing", "&averyverylongthing"),
            ("codepoint past U+10FFFF", "&#1114112;", "&#1114112;"),
            ("a surrogate is not a char", "&#xD800;", "&#xD800;"),
        ];
        for (why, input, expected) in cases {
            assert_eq!(decode_entities(input), *expected, "{why}");
        }
    }

    /// The adversarial one. A decoder that re-scans its own output expands this
    /// forever; a recursive one overflows the stack. One pass writes the literal text
    /// and stops.
    #[test]
    fn nested_entity_references_decode_exactly_once() {
        assert_eq!(decode_entities("&amp;lt;"), "&lt;");
        assert_eq!(decode_entities("&amp;amp;amp;lt;"), "&amp;amp;lt;");
        let bomb = "&amp;".repeat(10_000);
        assert_eq!(decode_entities(&bomb).len(), 10_000);
    }

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/"
     xmlns:dc="http://purl.org/dc/elements/1.1/">
  <channel>
    <title>Example Wire</title>
    <link>https://example.test</link>
    <description>Ignored at feed level</description>
    <item>
      <title>Regulator opens inquiry into the AT&amp;T merger</title>
      <link>https://example.test/a?utm_source=rss</link>
      <guid isPermaLink="false">tag:example.test,2026:1</guid>
      <pubDate>Mon, 10 Aug 2026 08:55:00 +0000</pubDate>
      <dc:creator>A. Reporter</dc:creator>
      <description>&lt;p&gt;The regulator said it would &lt;b&gt;review&lt;/b&gt; the deal.&lt;/p&gt;</description>
      <content:encoded><![CDATA[<p>Full text with <a href="/x">a link</a>.</p>]]></content:encoded>
    </item>
    <item>
      <title>Second item, no date</title>
      <link>https://example.test/b</link>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn rss_two_point_zero_reads_every_field_it_should() {
        let feed = parse_feed(RSS, NOW).expect("RSS must parse");
        assert_eq!(feed.kind, SourceKind::Rss);
        assert_eq!(feed.title.as_deref(), Some("Example Wire"));
        assert_eq!(feed.site_url.as_deref(), Some("https://example.test"));
        assert_eq!(feed.items.len(), 2);

        let first = &feed.items[0];
        // Entities decoded, and the RAW url kept — canonicalization is `canon`'s job
        // and the user clicks this one.
        assert_eq!(first.title, "Regulator opens inquiry into the AT&T merger");
        assert_eq!(first.url, "https://example.test/a?utm_source=rss");
        assert_eq!(first.guid.as_deref(), Some("tag:example.test,2026:1"));
        assert_eq!(first.author.as_deref(), Some("A. Reporter"));
        // The description was escaped HTML: decoded once by the XML layer, then
        // flattened by the HTML layer.
        assert_eq!(
            first.summary.as_deref(),
            Some("The regulator said it would review the deal.")
        );
        // CDATA is literal at the XML layer and HTML at the next one.
        assert_eq!(
            first.content.as_deref(),
            Some("Full text with a link.")
        );
        assert_eq!(first.published_at, 1_786_352_100_000);

        // No date anywhere: stamped with the `now` that was passed IN, which is what
        // makes a replay of these bytes reproducible.
        assert_eq!(feed.items[1].published_at, NOW);
    }

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Atom Example</title>
  <link rel="self" href="https://example.test/feed.xml"/>
  <link rel="alternate" href="https://example.test/"/>
  <updated>2026-08-10T09:00:00Z</updated>
  <entry>
    <title type="html">Chip &amp;amp; wafer export controls tighten</title>
    <link rel="alternate" type="text/html" href="https://example.test/atom-a"/>
    <link rel="enclosure" href="https://example.test/audio.mp3"/>
    <id>urn:uuid:1225c695</id>
    <published>2026-08-10T08:00:00Z</published>
    <updated>2026-08-10T08:30:00Z</updated>
    <author><name>B. Writer</name></author>
    <summary>Short summary.</summary>
    <content type="html">&lt;p&gt;Body text.&lt;/p&gt;</content>
  </entry>
</feed>"#;

    #[test]
    fn atom_one_point_zero_prefers_the_alternate_link_and_published_date() {
        let feed = parse_feed(ATOM, NOW).expect("Atom must parse");
        assert_eq!(feed.kind, SourceKind::Atom);
        assert_eq!(feed.title.as_deref(), Some("Atom Example"));
        // `rel="self"` is the feed document, not the site.
        assert_eq!(feed.site_url.as_deref(), Some("https://example.test/"));

        let entry = &feed.items[0];
        assert_eq!(entry.url, "https://example.test/atom-a");
        assert_eq!(entry.guid.as_deref(), Some("urn:uuid:1225c695"));
        assert_eq!(entry.author.as_deref(), Some("B. Writer"));
        assert_eq!(entry.summary.as_deref(), Some("Short summary."));
        assert_eq!(entry.content.as_deref(), Some("Body text."));
        // `published` wins over `updated`.
        assert_eq!(entry.published_at, 1_786_348_800_000);
        // Two rounds of escaping in the title, decoded twice: once by the XML layer,
        // then again by `html_to_text`, which every title/summary/content goes
        // through unconditionally (`Field::Title | Summary | Content` at the
        // `html_to_text` call site) rather than only when `type="html"` says to.
        //
        // That is deliberate. Feeds are not trustworthy about `type`: plenty declare
        // `text` and ship markup, and double-escaping is common enough that reading
        // the attribute would leave a literal `&amp;` on screen for a large fraction
        // of real sources. The cost is the reverse case — a `type="text"` title that
        // genuinely wanted to display the characters `&amp;` loses them — which is
        // both rarer and far less visible than the alternative.
        assert_eq!(entry.title, "Chip & wafer export controls tighten");
    }

    /// A feed that binds Atom to a prefix instead of the default namespace. Prefixes
    /// are dropped rather than resolved, so this reads identically.
    #[test]
    fn a_prefixed_atom_document_reads_the_same_as_an_unprefixed_one() {
        let prefixed = r#"<a:feed xmlns:a="http://www.w3.org/2005/Atom">
            <a:title>Prefixed</a:title>
            <a:entry>
              <a:title>Item</a:title>
              <a:link rel="alternate" href="https://example.test/p"/>
            </a:entry>
          </a:feed>"#;
        let feed = parse_feed(prefixed, NOW).expect("prefixed Atom must parse");
        assert_eq!(feed.kind, SourceKind::Atom);
        assert_eq!(feed.items.len(), 1);
        assert_eq!(feed.items[0].url, "https://example.test/p");
    }

    #[test]
    fn rss_one_point_zero_rdf_is_read_as_rss() {
        let rdf = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
                          xmlns="http://purl.org/rss/1.0/">
            <channel><title>RDF feed</title></channel>
            <item rdf:about="https://example.test/r">
              <title>RDF item</title>
              <link>https://example.test/r</link>
              <dc:date>2026-08-10T07:00:00Z</dc:date>
            </item>
          </rdf:RDF>"#;
        let feed = parse_feed(rdf, NOW).expect("RSS 1.0 must parse");
        assert_eq!(feed.kind, SourceKind::Rss);
        assert_eq!(feed.items.len(), 1);
        // 2026-08-10T07:00:00Z, read out of `dc:date` — RSS 1.0 has no `pubDate`.
        assert_eq!(feed.items[0].published_at, 1_786_345_200_000);
    }

    const JSON_FEED: &str = r#"{
      "version": "https://jsonfeed.org/version/1.1",
      "title": "JSON Example",
      "home_page_url": "https://example.test/",
      "items": [
        {
          "id": 4102,
          "url": "https://example.test/j1",
          "title": "A JSON item",
          "content_html": "<p>Body <em>here</em>.</p>",
          "date_published": "2026-08-10T06:00:00Z",
          "authors": [{ "name": "C. Author" }]
        },
        { "id": "no-url-so-dropped", "title": "Nowhere to click" }
      ]
    }"#;

    #[test]
    fn json_feed_rides_serde_and_drops_items_with_no_url() {
        let feed = parse_feed(JSON_FEED, NOW).expect("JSON Feed must parse");
        assert_eq!(feed.kind, SourceKind::JsonFeed);
        assert_eq!(feed.title.as_deref(), Some("JSON Example"));
        assert_eq!(feed.items.len(), 1, "the URL-less item must be dropped");
        let item = &feed.items[0];
        // A numeric `id` is still an id.
        assert_eq!(item.guid.as_deref(), Some("4102"));
        assert_eq!(item.content.as_deref(), Some("Body here."));
        assert_eq!(item.author.as_deref(), Some("C. Author"));
        // 2026-08-10T06:00:00Z.
        assert_eq!(item.published_at, 1_786_341_600_000);
    }

    /// THE adversarial case the algorithm doc names: malformed input must return an
    /// error — which the caller turns into `consecutive_failures += 1` and a backoff —
    /// and must never panic, hang, or overflow the stack.
    #[test]
    fn malformed_input_table() {
        let cases: &[(&str, &str, FeedError)] = &[
            ("empty body", "", FeedError::Empty),
            ("whitespace only", "   \n\t ", FeedError::Empty),
            ("an HTML page instead of a feed", "<!DOCTYPE html><html><body>Login</body></html>", FeedError::UnknownFormat),
            ("plain prose", "not xml at all", FeedError::UnknownFormat),
            ("angle-bracket soup with no element", "<<<>>>", FeedError::UnknownFormat),
            ("JSON that is not a feed", "{\"hello\":\"world\"}", FeedError::UnknownFormat),
            ("truncated JSON", "{\"items\": [", FeedError::Json(String::new())),
            ("a feed cut inside its first tag", "<rss><channel><item><title attr=\"unterminated", FeedError::Malformed(String::new())),
        ];
        for (why, body, expected) in cases {
            let err = parse_feed(body, NOW).expect_err(why);
            // Compare the VARIANT, not the message: the messages are for a human
            // reading the source list and are free to change.
            assert_eq!(
                std::mem::discriminant(&err),
                std::mem::discriminant(expected),
                "{why}: got {err:?}"
            );
        }
    }

    /// A cut connection is not a broken feed. The items that arrived are real, and
    /// throwing away forty good ones because the forty-first was truncated would make
    /// a flaky network look like a dead source.
    #[test]
    fn a_truncated_download_keeps_the_items_that_arrived() {
        let body = "<rss><channel>\
            <item><title>One</title><link>https://example.test/1</link></item>\
            <item><title>Two</title><link>https://example.test/2</link></item>\
            <item><title>Three</title><link>https://exam";
        let feed = parse_feed(body, NOW).expect("a partial feed still parses");
        assert!(feed.truncated);
        assert_eq!(feed.items.len(), 2);
        assert_eq!(feed.items[1].url, "https://example.test/2");
    }

    /// Deep nesting is the case that would abort the whole test process if the walk
    /// were recursive: a stack overflow is not a catchable panic.
    #[test]
    fn pathological_nesting_is_an_error_not_a_stack_overflow() {
        let mut body = String::from("<rss>");
        for _ in 0..50_000 {
            body.push_str("<a>");
        }
        let err = parse_feed(&body, NOW).expect_err("depth must be refused");
        assert!(matches!(err, FeedError::Malformed(_)));
    }

    /// An unbounded feed must not become an unbounded allocation.
    #[test]
    fn item_count_is_capped() {
        let mut body = String::from("<rss><channel>");
        for i in 0..(MAX_ITEMS_PER_FEED + 50) {
            body.push_str(&format!(
                "<item><title>{i}</title><link>https://example.test/{i}</link></item>"
            ));
        }
        body.push_str("</channel></rss>");
        let feed = parse_feed(&body, NOW).expect("must parse");
        assert_eq!(feed.items.len(), MAX_ITEMS_PER_FEED);
    }

    #[test]
    fn date_parsing_table() {
        let cases: &[(&str, &str, Option<i64>)] = &[
            ("RFC 2822 with numeric offset", "Mon, 10 Aug 2026 08:55:00 +0000", Some(1_786_352_100_000)),
            ("RFC 2822 with GMT", "Mon, 10 Aug 2026 08:55:00 GMT", Some(1_786_352_100_000)),
            ("RFC 2822 with a trailing comment", "Mon, 10 Aug 2026 08:55:00 +0000 (UTC)", Some(1_786_352_100_000)),
            ("RFC 3339 with Z", "2026-08-10T08:55:00Z", Some(1_786_352_100_000)),
            ("RFC 3339 with an offset", "2026-08-10T10:55:00+02:00", Some(1_786_352_100_000)),
            ("naive datetime, read as UTC", "2026-08-10 08:55:00", Some(1_786_352_100_000)),
            ("date only", "2026-08-10", Some(1_786_320_000_000)),
            ("nonsense", "last Tuesday", None),
            ("empty", "   ", None),
        ];
        for (why, raw, expected) in cases {
            assert_eq!(parse_date(raw), *expected, "{why}");
        }
    }

    /// The scanner is shared with `extract`, so its own edge cases are pinned here.
    #[test]
    fn the_scanner_survives_the_shapes_that_break_naive_ones() {
        // A `>` inside a quoted attribute does not end the tag.
        let mut s = Scanner::new("<a title=\"x > y\">t</a>");
        assert!(matches!(s.next_token(), Some(Token::Start { .. })));
        assert_eq!(s.next_token(), Some(Token::Text("t")));
        // CDATA comes back as its own token, undecoded.
        let mut s = Scanner::new("<![CDATA[a &amp; b]]>");
        assert_eq!(s.next_token(), Some(Token::CData("a &amp; b")));
        // Comments and processing instructions are skipped without recursing.
        // Bound to a local: `Scanner` borrows its input, so inlining the `repeat`
        // would drop the String at the end of the statement that created it.
        let many_comments = "<!-- c -->".repeat(20_000);
        let mut s = Scanner::new(&many_comments);
        assert_eq!(s.next_token(), None);
        assert!(!s.truncated);
        // An unterminated tag marks the scan truncated rather than panicking.
        let mut s = Scanner::new("<a");
        assert_eq!(s.next_token(), None);
        assert!(s.truncated);
    }

    #[test]
    fn attributes_parse_through_quotes_spaces_and_missing_values() {
        assert_eq!(attr("href=\"https://x.test/a\" rel=\"alternate\"", "href"), Some("https://x.test/a"));
        assert_eq!(attr("href='https://x.test/b'", "HREF"), Some("https://x.test/b"));
        assert_eq!(attr("href = https://x.test/c type=text/html", "href"), Some("https://x.test/c"));
        assert_eq!(attr("rel=\"self\"", "href"), None);
        assert_eq!(attr("hidden href=\"y\"", "href"), Some("y"));
        assert_eq!(local_name("content:encoded"), "encoded");
        assert_eq!(local_name("DIV"), "div");
    }
}
