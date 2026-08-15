//! HTML to text, by hand, plus the text-density pick of the main-content subtree.
//!
//! Feeds carry HTML in three places (`<description>`, `<content:encoded>`, JSON
//! Feed's `content_html`) and the `web.extract` capability returns a whole page for
//! sources with no usable feed. All four have to become plain text before anything
//! downstream — shingling, entity extraction, topic matching, the snapshot — can look
//! at them.
//!
//! # Why this is written out rather than pulled in
//!
//! `scraper` / `html5ever` are not in the root `Cargo.lock` and this app may not put
//! them there (see the dependency rule in `Cargo.toml`). Dragging a dozen transitive
//! crates into a lockfile several jobs share, to strip some tags, is not a trade
//! worth making.
//!
//! # Three shapes of hostile input, three explicit defences
//!
//! This code runs over bytes fetched from servers nobody here controls, so the
//! failure modes are not hypothetical:
//!
//! 1. **Deep nesting** — a page with 50 000 nested `<div>`s. Every walk here is
//!    ITERATIVE with an explicit stack and a [`MAX_HTML_DEPTH`] cap. A recursive
//!    descent would blow the stack, and a stack overflow is not a catchable panic:
//!    it aborts the process, so "returns an error instead" would not even be
//!    testable.
//! 2. **Entity bombs** — `&amp;amp;amp;…` expanded repeatedly. [`decode_entities`]
//!    is a SINGLE pass that never re-scans what it has already written, so
//!    `&amp;lt;` decodes to the four characters `&lt;` and stops there.
//! 3. **Unterminated tags** — `<script>` with no `</script>`, a `<` that is never
//!    closed. Every skip-ahead terminates at end-of-input rather than running off
//!    or looping.

use std::collections::BTreeMap;

/// Hard ceiling on element nesting. Past it the walk stops descending and treats the
/// rest as text — a page nested deeper than this is not an article.
pub const MAX_HTML_DEPTH: usize = 256;

/// Longest entity reference [`decode_entities`] will look ahead for, in bytes.
/// `&thetasym;` is 9; a `&` followed by 200 characters and then a `;` is prose.
pub const MAX_ENTITY_LEN: usize = 12;

/// A subtree must hold at least this much text before it can be chosen as the main
/// content. Below it, the whole document is used instead: a 40-character `<div>` that
/// happens to have the best density is a byline, not an article.
pub const MIN_MAIN_TEXT_CHARS: usize = 200;

/// Subtrees dropped whole, contents included. Their text is code and stylesheet
/// source, and one un-dropped `<script>` puts a JSON blob into the article's
/// shingles, which is enough to make two unrelated pages look like duplicates.
const DROPPED_SUBTREES: &[&str] = &["noscript", "script", "style", "svg", "template"];

/// Elements that force a line break, so `<p>a</p><p>b</p>` becomes two lines rather
/// than `ab` — which would otherwise create the shingle "a b" out of two unrelated
/// sentences.
const BLOCK_TAGS: &[&str] = &[
    "address", "article", "aside", "blockquote", "br", "dd", "div", "dl", "dt",
    "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6",
    "header", "hr", "li", "main", "nav", "ol", "p", "pre", "section", "table", "tbody",
    "td", "tfoot", "th", "thead", "tr", "ul",
];

/// Block elements that end a LINE rather than a paragraph: they break on open and
/// not again on close, so a five-item list is five lines instead of five lines with
/// a blank line between each.
///
/// Every entry is also in [`BLOCK_TAGS`] — this list only decides whether the CLOSE
/// tag emits a second newline. Sorted; looked up by binary search.
const LINE_TAGS: &[&str] = &["br", "dd", "dt", "li", "td", "th", "tr"];

/// Elements with no closing tag. They must not push a frame, or every `<br>` leaves
/// an open element behind and the density spans become nonsense.
const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta",
    "param", "source", "track", "wbr",
];

/// Elements eligible to BE the main content.
///
/// Restricted to containers on purpose. With the density score below, an
/// unrestricted candidate set lets a single 400-character `<p>` (score 400) beat the
/// `<article>` that contains it and thirty other paragraphs (score 2000/31 ≈ 65) —
/// and picking one paragraph out of a story is a worse answer than picking the whole
/// page.
const CONTAINER_TAGS: &[&str] = &["article", "aside", "blockquote", "body", "div", "main", "section", "td"];

fn in_sorted(list: &[&str], name: &str) -> bool {
    list.binary_search(&name).is_ok()
}

// ── Entities ───────────────────────────────────────────────────────────────────

/// The named references worth carrying. Sorted; looked up by binary search.
///
/// Not the full HTML5 table (2231 entries, most of them mathematical): these are the
/// ones that actually appear in headlines and article bodies. An unrecognized
/// reference is left EXACTLY as written rather than dropped, so `Q&A` survives and a
/// `&hearts;` shows up as itself instead of silently vanishing from the text.
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

/// Decode HTML/XML character references in ONE pass.
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
        // Safe: `i` only ever advances by a whole char or past ASCII bytes.
        let ch = rest.chars().next().unwrap_or('&');
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

// ── Tokenizer ──────────────────────────────────────────────────────────────────

/// One thing the tokenizer found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token<'a> {
    Start { name: String, attrs: &'a str, self_closing: bool },
    End { name: String },
    Text(&'a str),
}

/// A forward-only HTML scanner. No tree, no allocation per node: the walkers below
/// keep whatever state they need on their own explicit stacks.
struct Scanner<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    /// Position of `needle` at or after `from`, or end-of-input.
    ///
    /// Every skip in this file goes through here so that "not found" means "consume
    /// the rest" rather than an unbounded loop or a slice panic.
    fn find_from(&self, from: usize, needle: &str) -> Option<usize> {
        self.src.get(from..)?.find(needle).map(|idx| from + idx)
    }

    /// End of the tag starting at `self.pos`, honouring quoted attribute values so a
    /// `>` inside `alt="a > b"` does not truncate the tag.
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

    fn next_token(&mut self) -> Option<Token<'a>> {
        if self.pos >= self.src.len() {
            return None;
        }
        let bytes = self.src.as_bytes();
        if bytes[self.pos] != b'<' {
            let end = self.find_from(self.pos + 1, "<").unwrap_or(self.src.len());
            let text = &self.src[self.pos..end];
            self.pos = end;
            return Some(Token::Text(text));
        }

        let rest = &self.src[self.pos..];
        if rest.starts_with("<!--") {
            // Unterminated comment: everything after it is comment. Terminating at
            // end-of-input rather than erroring keeps a half-downloaded page usable.
            let end = self
                .find_from(self.pos + 4, "-->")
                .map_or(self.src.len(), |i| i + 3);
            self.pos = end;
            return self.next_token();
        }
        if rest.starts_with("<![CDATA[") {
            let start = self.pos + 9;
            let (text_end, next) = match self.find_from(start, "]]>") {
                Some(i) => (i, i + 3),
                None => (self.src.len(), self.src.len()),
            };
            let text = &self.src[start..text_end];
            self.pos = next;
            return Some(Token::Text(text));
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            let end = self.tag_end(self.pos).map_or(self.src.len(), |i| i + 1);
            self.pos = end;
            return self.next_token();
        }

        let is_close = rest.starts_with("</");
        let name_start = self.pos + if is_close { 2 } else { 1 };
        let first = self.src[name_start..].chars().next();
        // A `<` that does not start a tag is text — `5 < 6` in a headline is common
        // and must not eat the rest of the document.
        if !first.is_some_and(|c| c.is_ascii_alphabetic()) {
            let end = self.find_from(self.pos + 1, "<").unwrap_or(self.src.len());
            let text = &self.src[self.pos..end];
            self.pos = end;
            return Some(Token::Text(text));
        }

        let Some(gt) = self.tag_end(name_start) else {
            // Unterminated tag at end of input: the remainder is markup, not text.
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
        Some(if is_close {
            Token::End { name }
        } else {
            Token::Start { name, attrs, self_closing }
        })
    }
}

/// Lowercased local name: `content:encoded` → `encoded`, `DIV` → `div`.
///
/// Namespace prefixes are DROPPED rather than matched, which is the "loose namespace
/// handling" the feed parser needs — a feed that binds Atom to `a:` instead of the
/// default namespace is otherwise unreadable, and every real-world combination of
/// prefixes is not a set anyone can enumerate.
fn local_name(raw: &str) -> String {
    let local = raw.rsplit(':').next().unwrap_or(raw);
    local.trim().to_ascii_lowercase()
}

// ── HTML → text ────────────────────────────────────────────────────────────────

/// One element's extent in the produced text, for the density pick.
#[derive(Debug, Clone)]
struct Span {
    start: usize,
    end: usize,
    tags: usize,
}

struct Frame {
    name: String,
    text_start: usize,
    tags_before: usize,
}

/// The raw text of a document plus the spans the density heuristic scores.
struct Walked {
    text: String,
    /// Keyed by span start so the selection order is deterministic without a sort
    /// over floats. A `BTreeMap` rather than a `HashMap` because iteration order
    /// reaching a result is exactly what makes a "why is this the main content"
    /// answer unreproducible.
    spans: BTreeMap<usize, Span>,
}

fn walk(html: &str) -> Walked {
    let mut scanner = Scanner::new(html);
    let mut text = String::with_capacity(html.len() / 2);
    let mut stack: Vec<Frame> = Vec::new();
    let mut spans: BTreeMap<usize, Span> = BTreeMap::new();
    let mut tags = 0usize;

    while let Some(token) = scanner.next_token() {
        match token {
            Token::Text(raw) => {
                let decoded = decode_entities(raw);
                if !decoded.is_empty() {
                    text.push_str(&decoded);
                }
            }
            Token::Start { name, self_closing, .. } => {
                tags += 1;
                if in_sorted(DROPPED_SUBTREES, &name) {
                    if !self_closing {
                        skip_subtree(&mut scanner, &name);
                    }
                    continue;
                }
                if in_sorted(BLOCK_TAGS, &name) {
                    text.push('\n');
                }
                if self_closing || in_sorted(VOID_TAGS, &name) {
                    continue;
                }
                // Past the cap the element is not tracked, so the stack cannot grow
                // without bound on a pathologically nested page. Its text still
                // lands in the buffer.
                if stack.len() < MAX_HTML_DEPTH {
                    stack.push(Frame {
                        name,
                        text_start: text.len(),
                        tags_before: tags,
                    });
                }
            }
            Token::End { name } => {
                // A block element breaks on open AND on close, which is what puts a
                // blank line between two paragraphs. Line-level elements
                // (`LINE_TAGS`) break only on open — otherwise every list item and
                // every table row would be double-spaced.
                if in_sorted(BLOCK_TAGS, &name) && !in_sorted(LINE_TAGS, &name) {
                    text.push('\n');
                }
                // Pop to the matching open element, closing everything left dangling
                // in between. An end tag with no opener is ignored rather than
                // treated as an error — unbalanced markup is the norm, not the
                // exception.
                if let Some(idx) = stack.iter().rposition(|f| f.name == name) {
                    while stack.len() > idx {
                        let frame = stack.pop().expect("rposition guarantees non-empty");
                        close_frame(&frame, &text, tags, &mut spans);
                    }
                }
            }
        }
    }
    // Unclosed elements at end of input still count: a truncated download is the
    // common case, and dropping every span would fall back to the whole document.
    while let Some(frame) = stack.pop() {
        close_frame(&frame, &text, tags, &mut spans);
    }
    Walked { text, spans }
}

fn close_frame(frame: &Frame, text: &str, tags: usize, spans: &mut BTreeMap<usize, Span>) {
    if !in_sorted(CONTAINER_TAGS, &frame.name) {
        return;
    }
    let span = Span {
        start: frame.text_start,
        end: text.len(),
        tags: tags.saturating_sub(frame.tags_before),
    };
    // The OUTERMOST element wins a shared start offset: it has at least as much text
    // and its density is the one that says whether the wrapper or the wrapped is the
    // article. Insert only when absent, and pops run innermost-first, so this is a
    // deliberate last-write-wins on the outer frame.
    spans.insert(span.start, span);
}

/// Consume everything up to the matching close tag of `name`, or to end of input.
///
/// Iterative and EOF-terminated: `<script>` with no `</script>` consumes the rest of
/// the document, which is the correct reading and — more importantly — terminates.
fn skip_subtree(scanner: &mut Scanner<'_>, name: &str) {
    let mut depth = 1usize;
    while let Some(token) = scanner.next_token() {
        match token {
            Token::Start { name: n, self_closing, .. } if n == name && !self_closing => {
                depth += 1;
            }
            Token::End { name: n } if n == name => {
                depth -= 1;
                if depth == 0 {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// Collapse the raw buffer into readable text: horizontal whitespace runs to one
/// space, three or more newlines to two, trimmed.
fn tidy(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_newlines = 0usize;
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch == '\n' || ch == '\r' {
            pending_newlines += 1;
            pending_space = false;
            continue;
        }
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if !out.is_empty() {
            if pending_newlines > 0 {
                for _ in 0..pending_newlines.min(2) {
                    out.push('\n');
                }
            } else if pending_space {
                out.push(' ');
            }
        }
        pending_newlines = 0;
        pending_space = false;
        out.push(ch);
    }
    out
}

/// Every readable character in the document, with block-level line breaks.
pub fn html_to_text(html: &str) -> String {
    tidy(&walk(html).text)
}

/// The main-content subtree's text, by density.
///
/// `score = text_len / (1 + tag_count)`: an article body is a lot of text under few
/// tags, a navigation column is a little text under many. The candidate set is
/// containers only (see [`CONTAINER_TAGS`]), the winner must clear
/// [`MIN_MAIN_TEXT_CHARS`], and a document with no qualifying subtree falls back to
/// [`html_to_text`] — a plain-text body, an AMP page, or a feed description has no
/// containers to choose between and must not come back empty.
///
/// Ties break toward the LONGER text and then the EARLIER start offset, so the answer
/// does not depend on iteration order.
pub fn extract_main_text(html: &str) -> String {
    let walked = walk(html);
    let mut best: Option<(f64, usize, usize)> = None; // (score, text_len, start)
    for span in walked.spans.values() {
        let slice = walked.text.get(span.start..span.end).unwrap_or("");
        let len = slice.trim().len();
        if len < MIN_MAIN_TEXT_CHARS {
            continue;
        }
        let score = len as f64 / (1 + span.tags) as f64;
        let better = match best {
            None => true,
            Some((best_score, best_len, best_start)) => {
                score > best_score
                    || (score == best_score
                        && (len > best_len || (len == best_len && span.start < best_start)))
            }
        };
        if better {
            best = Some((score, len, span.start));
        }
    }
    match best {
        Some((_, _, start)) => {
            let span = &walked.spans[&start];
            tidy(walked.text.get(span.start..span.end).unwrap_or(""))
        }
        None => tidy(&walked.text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_table_is_sorted_for_the_binary_search() {
        let mut sorted: Vec<&str> = NAMED_ENTITIES.iter().map(|(n, _)| *n).collect();
        sorted.sort_unstable();
        let actual: Vec<&str> = NAMED_ENTITIES.iter().map(|(n, _)| *n).collect();
        assert_eq!(actual, sorted);
        for list in [
            DROPPED_SUBTREES,
            BLOCK_TAGS,
            LINE_TAGS,
            VOID_TAGS,
            CONTAINER_TAGS,
        ] {
            let mut s = list.to_vec();
            s.sort_unstable();
            assert_eq!(list, s.as_slice(), "tag list is not sorted");
        }
        // `LINE_TAGS` only ever SUPPRESSES the closing newline a block tag would
        // emit, so an entry that is not also a block tag would do nothing at all and
        // read as though it did.
        for name in LINE_TAGS {
            assert!(
                in_sorted(BLOCK_TAGS, name),
                "LINE_TAGS entry '{name}' is not a block tag, so it has no effect"
            );
        }
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
            ("no semicolon within the window", "&averyverylongthing", "&averyverylongthing"),
            ("out-of-range codepoint is left alone", "&#1114112;", "&#1114112;"),
            ("a surrogate is not a char", "&#xD800;", "&#xD800;"),
        ];
        for (why, input, expected) in cases {
            assert_eq!(decode_entities(input), *expected, "{why}");
        }
    }

    /// The adversarial one. A decoder that re-scans its own output expands this
    /// forever; a recursive one overflows the stack. One pass produces the literal
    /// text and stops.
    #[test]
    fn nested_entity_references_decode_exactly_once() {
        assert_eq!(decode_entities("&amp;lt;"), "&lt;");
        assert_eq!(decode_entities("&amp;amp;amp;lt;"), "&amp;amp;lt;");
        // A long chain terminates in linear time and shrinks rather than grows.
        let bomb = "&amp;".repeat(10_000);
        let decoded = decode_entities(&bomb);
        assert_eq!(decoded.len(), 10_000);
    }

    #[test]
    fn script_style_and_noscript_subtrees_are_dropped_whole() {
        let html = "<div><script>var a = 1 < 2;</script><p>Kept</p>\
                    <style>.a{color:red}</style><noscript>Enable JS</noscript></div>";
        let text = html_to_text(html);
        assert_eq!(text, "Kept");
        assert!(!text.contains("var a"));
        assert!(!text.contains("color"));
        assert!(!text.contains("Enable JS"));
    }

    /// The termination case: markup that never closes must consume the rest and
    /// return, not loop.
    #[test]
    fn unterminated_markup_terminates() {
        assert_eq!(html_to_text("<p>Before<script>never closed"), "Before");
        assert_eq!(html_to_text("<p>Before<!-- never closed"), "Before");
        assert_eq!(html_to_text("<p>Before<div class=\"x"), "Before");
        assert_eq!(html_to_text("<![CDATA[unterminated"), "unterminated");
        // A stray `<` in prose is text, and does not eat the document.
        assert_eq!(html_to_text("<p>5 < 6 is true</p>"), "5 < 6 is true");
    }

    /// Deep nesting is the case that would abort the test process if the walk were
    /// recursive — a stack overflow is not a catchable panic.
    #[test]
    fn fifty_thousand_nested_divs_do_not_blow_the_stack() {
        let mut html = String::new();
        for _ in 0..50_000 {
            html.push_str("<div>");
        }
        html.push_str("deep");
        for _ in 0..50_000 {
            html.push_str("</div>");
        }
        assert_eq!(html_to_text(&html), "deep");
    }

    #[test]
    fn block_tags_become_line_breaks_and_inline_tags_do_not() {
        // A paragraph boundary is a BLANK line and a line break is a single one, so
        // the extracted text keeps the shape a reader would see. That distinction is
        // not cosmetic here: the density heuristic scores candidate subtrees by text
        // per line, and collapsing paragraphs to single newlines would make a wall of
        // short paragraphs look exactly like a nav list.
        assert_eq!(html_to_text("<p>one</p><p>two</p>"), "one\n\ntwo");
        assert_eq!(html_to_text("a<br>b"), "a\nb");
        assert_eq!(html_to_text("<b>bold</b> and <i>italic</i>"), "bold and italic");
        assert_eq!(html_to_text("<ul><li>a</li><li>b</li></ul>"), "a\nb");
    }

    /// The density pick: a page whose article body is surrounded by a link-dense
    /// navigation column and a footer.
    #[test]
    fn density_picks_the_article_over_the_navigation() {
        let body = "The regulator opened a formal inquiry into the merger on Monday, \
                    saying the combined entity would control more than half of the \
                    domestic market for advanced packaging. The companies said they \
                    would cooperate fully with the review and expected it to conclude \
                    within the year.";
        let nav: String = (0..40)
            .map(|i| format!("<li><a href=\"/s/{i}\">Section {i}</a></li>"))
            .collect();
        let html = format!(
            "<body><nav><ul>{nav}</ul></nav><div id=\"main\"><article><p>{body}</p></article></div>\
             <footer><div><a href=\"/tos\">Terms</a><a href=\"/privacy\">Privacy</a></div></footer></body>"
        );
        let main = extract_main_text(&html);
        assert!(main.contains("advanced packaging"), "main was: {main}");
        assert!(!main.contains("Section 7"), "navigation leaked in: {main}");
        assert!(!main.contains("Privacy"));
    }

    /// Below the floor there is nothing worth choosing, and returning a byline
    /// instead of the page would be worse than returning the page.
    #[test]
    fn a_document_with_no_qualifying_subtree_falls_back_to_the_whole_text() {
        assert_eq!(extract_main_text("<p>Too short to be an article.</p>"), "Too short to be an article.");
        assert_eq!(extract_main_text("just plain text"), "just plain text");
        assert_eq!(extract_main_text(""), "");
    }
}

