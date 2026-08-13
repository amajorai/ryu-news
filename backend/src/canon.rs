//! Dedupe layer 1: URL canonicalization.
//!
//! The same article reached through six share links is six URLs and one page. The
//! canonical form is what `articles(workspace_id, canonical_url)` — a real UNIQUE
//! index — collapses them on. It is a **key, never a link**:
//! [`crate::models::Article::url`] keeps the URL exactly as published, because that
//! is what the user clicks and stripping a query param out of it is how a paywall
//! token or a required `?id=` goes missing.
//!
//! # Two things this module deliberately does NOT do
//!
//! - **It does not follow redirects.** A link shim (`t.co`, `news.google.com/rss/…`)
//!   is a different URL per share of the same article, so the terminal URL is the one
//!   worth canonicalizing — but resolving it is a network call, and
//!   [`crate::state::build_http_client`] already does it with
//!   `redirect::Policy::limited(10)` and says why. This module is pure and offline:
//!   it takes the string the fetch layer ended on.
//! - **It does not use a URL crate.** `url` is not in the root `Cargo.lock` and this
//!   app may not put it there, so the parse below is by hand. It is deliberately
//!   lenient — a canonicalizer that rejects an odd URL would drop the article, and a
//!   feed full of odd URLs is Tuesday. Anything unparseable degrades to
//!   [`crate::text::normalize`] of the input, which still dedupes a URL against
//!   itself and against its own whitespace variants.
//!
//! Percent-encoding is left byte-for-byte alone. Re-encoding it correctly needs a
//! table of which characters are safe per component, and getting that subtly wrong
//! would merge two genuinely different URLs — which is worse than missing a dedupe.

use crate::text::normalize;

/// Query parameters removed before the canonical form is built.
///
/// Sorted, and looked up with a binary search. Every entry is a parameter that
/// identifies *how you arrived*, never *what you are looking at* — which is the test
/// for adding one. `s` and `ref` are the aggressive members of the set (some sites do
/// use `?s=` for a search term), and they are here because ALGORITHMS names them: a
/// share link that differs only in `?ref=twitter` is the single most common duplicate
/// in a feed.
pub const TRACKING_PARAMS: &[&str] = &[
    "at_campaign",
    "at_medium",
    "fbclid",
    "gclid",
    "igshid",
    "mc_eid",
    "ref",
    "ref_src",
    "s",
    "spm",
];

/// Prefix form of the same rule: every `utm_*` parameter, whatever the suffix.
pub const TRACKING_PARAM_PREFIX: &str = "utm_";

/// Is this query key a tracking parameter?
pub fn is_tracking_param(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    lowered.starts_with(TRACKING_PARAM_PREFIX) || TRACKING_PARAMS.binary_search(&&*lowered).is_ok()
}

/// A URL split into the pieces canonicalization touches.
///
/// Borrowed rather than owned: the only mutation is lowercasing, and that happens on
/// the way out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl<'a> {
    pub scheme: &'a str,
    /// `user:pass@` including the `@`, or empty. Preserved verbatim — dropping it
    /// would merge two URLs that authenticate differently.
    pub userinfo: &'a str,
    pub host: &'a str,
    pub port: Option<&'a str>,
    /// Includes the leading `/`, or empty when the URL had no path at all.
    pub path: &'a str,
    /// Without the `?`.
    pub query: &'a str,
    /// Without the `#`. Always dropped by [`canonicalize`]; parsed so a caller can
    /// see what was there.
    pub fragment: &'a str,
}

/// The port that is implied by the scheme and therefore never written.
fn default_port(scheme: &str) -> Option<&'static str> {
    match scheme {
        "http" => Some("80"),
        "https" => Some("443"),
        _ => None,
    }
}

/// Split an absolute URL. `None` when there is no `scheme://` — a relative reference
/// has no canonical form of its own, and guessing a base is how an article ends up
/// keyed to the wrong site.
pub fn parse(raw: &str) -> Option<ParsedUrl<'_>> {
    let trimmed = raw.trim();
    let (scheme, rest) = trimmed.split_once("://")?;
    if scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
        return None;
    }

    // Authority ends at the first `/`, `?` or `#`, whichever comes first.
    let authority_end = rest
        .find(['/', '?', '#'])
        .unwrap_or(rest.len());
    let (authority, after) = rest.split_at(authority_end);

    // `@` splits userinfo from host. `rfind`, not `find`: a password may legally
    // contain an `@`, and the LAST one is the delimiter.
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(idx) => authority.split_at(idx + 1),
        None => ("", authority),
    };
    if hostport.is_empty() {
        return None;
    }

    // An IPv6 literal is bracketed and full of colons, so the port split has to
    // happen after the closing bracket or `[::1]:8080` parses as host `[` .
    let (host, port) = match hostport.rfind(']') {
        Some(close) => match hostport[close + 1..].strip_prefix(':') {
            Some(p) => (&hostport[..=close], Some(p)),
            None => (hostport, None),
        },
        None => match hostport.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        },
    };
    if host.is_empty() {
        return None;
    }

    let (before_fragment, fragment) = match after.split_once('#') {
        Some((b, f)) => (b, f),
        None => (after, ""),
    };
    let (path, query) = match before_fragment.split_once('?') {
        Some((p, q)) => (p, q),
        None => (before_fragment, ""),
    };

    Some(ParsedUrl {
        scheme,
        userinfo,
        host,
        port: port.filter(|p| !p.is_empty()),
        path,
        query,
        fragment,
    })
}

/// Split a query string into `(key, value_with_eq_or_empty)` pairs, preserving the
/// raw spelling of each pair so the rebuilt string is byte-identical to the input
/// minus what was removed.
fn query_pairs(query: &str) -> Vec<(String, &str)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let key = pair.split_once('=').map_or(pair, |(k, _)| k);
            (key.to_ascii_lowercase(), pair)
        })
        .collect()
}

/// The dedupe key for a URL.
///
/// Lowercase scheme and host, drop the default port, drop the fragment, drop the
/// tracking parameters, sort what survives, drop one trailing `/`.
///
/// Total by construction — every article needs a key, and a `Result` here would push
/// a decision onto every caller that all of them would answer the same way. A URL
/// this cannot parse degrades to its normalized self, which still deduplicates it
/// against its own reposts.
pub fn canonicalize(raw: &str) -> String {
    let Some(url) = parse(raw) else {
        return normalize(raw);
    };

    let scheme = url.scheme.to_ascii_lowercase();
    // ASCII-lowercase for the host, not `to_lowercase`: a hostname is ASCII (an IDN
    // arrives already punycoded), and full case mapping would fold a Turkish dotless
    // ı into something that is not the host anyone typed.
    let host = url.host.to_ascii_lowercase();

    let mut out = String::with_capacity(raw.len());
    out.push_str(&scheme);
    out.push_str("://");
    out.push_str(url.userinfo);
    out.push_str(&host);
    if let Some(port) = url.port {
        if Some(port) != default_port(&scheme) {
            out.push(':');
            out.push_str(port);
        }
    }

    // Strip ONE trailing slash. `/a/b/` and `/a/b` are the same page everywhere that
    // matters; `//` is not, and neither is the root, which becomes an empty path
    // rather than a bare `/` so `https://x.test` and `https://x.test/` agree.
    let path = url.path.strip_suffix('/').unwrap_or(url.path);
    out.push_str(path);

    let mut pairs: Vec<(String, &str)> = query_pairs(url.query)
        .into_iter()
        .filter(|(key, _)| !is_tracking_param(key))
        .collect();
    // Sort by (key, raw pair) so `?b=1&a=2` and `?a=2&b=1` agree and two values of
    // the same key keep a stable order among themselves.
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    if !pairs.is_empty() {
        out.push('?');
        for (idx, (_, pair)) in pairs.iter().enumerate() {
            if idx > 0 {
                out.push('&');
            }
            out.push_str(pair);
        }
    }
    // The fragment is never written: `#comments` and `#:~:text=` are positions inside
    // one page, not different pages.
    out
}

/// Does this string look like an absolute `http(s)` URL?
///
/// The check a route runs before storing a feed URL, so a typo is a 400 with a
/// message rather than a source that fails every poll for a week.
pub fn is_http_url(raw: &str) -> bool {
    parse(raw).is_some_and(|u| {
        let scheme = u.scheme.to_ascii_lowercase();
        scheme == "http" || scheme == "https"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table-driven: raw in, canonical out. Each row is a real shape a feed produces.
    #[test]
    fn canonicalization_table() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "scheme and host are lowercased, the fragment goes",
                "HTTPS://Example.COM/Story#comments",
                "https://example.com/Story",
            ),
            (
                "the path keeps its case — paths are case-sensitive on most servers",
                "https://example.com/Story/Part-Two",
                "https://example.com/Story/Part-Two",
            ),
            (
                "the default port is implied and never written",
                "https://example.com:443/a",
                "https://example.com/a",
            ),
            (
                "a non-default port is part of the identity",
                "https://example.com:8443/a",
                "https://example.com:8443/a",
            ),
            (
                "http's default port is 80, not 443",
                "http://example.com:80/a",
                "http://example.com/a",
            ),
            (
                "one trailing slash goes, and the root path becomes empty",
                "https://example.com/",
                "https://example.com",
            ),
            (
                "utm_* is stripped whatever the suffix",
                "https://example.com/a?utm_source=x&utm_content=y&id=7",
                "https://example.com/a?id=7",
            ),
            (
                "the named tracking params are stripped too",
                "https://example.com/a?fbclid=1&gclid=2&ref=twitter&ref_src=tw&s=09&spm=z&igshid=q&mc_eid=e&at_medium=rss&at_campaign=c&id=7",
                "https://example.com/a?id=7",
            ),
            (
                "surviving pairs are sorted, so param order stops mattering",
                "https://example.com/a?b=2&a=1&c=3",
                "https://example.com/a?a=1&b=2&c=3",
            ),
            (
                "a query that was ALL tracking leaves no `?` behind",
                "https://example.com/a?utm_source=newsletter",
                "https://example.com/a",
            ),
            (
                "a valueless flag param survives as itself",
                "https://example.com/a?amp&id=7",
                "https://example.com/a?amp&id=7",
            ),
            (
                "IPv6 literals keep their brackets and lose their default port",
                "https://[2001:DB8::1]:443/a",
                "https://[2001:db8::1]/a",
            ),
            (
                "userinfo is preserved — two credentials are two URLs",
                "https://user:pw@example.com/a",
                "https://user:pw@example.com/a",
            ),
        ];
        for (why, raw, expected) in cases {
            assert_eq!(canonicalize(raw), *expected, "{why}");
        }
    }

    /// The whole point of layer 1: six share links, one key.
    #[test]
    fn six_share_links_to_one_article_collapse_to_one_key() {
        let variants = [
            "https://example.com/story",
            "https://example.com/story/",
            "https://EXAMPLE.com/story?utm_source=twitter&utm_medium=social",
            "https://example.com/story#top",
            "https://example.com:443/story?fbclid=IwAR123",
            "  https://example.com/story?ref=newsletter  ",
        ];
        let keys: Vec<String> = variants.iter().map(|v| canonicalize(v)).collect();
        assert!(
            keys.windows(2).all(|w| w[0] == w[1]),
            "share links did not collapse: {keys:?}"
        );
        assert_eq!(keys[0], "https://example.com/story");
    }

    /// The distinctions that must NOT collapse. A canonicalizer that over-merges
    /// silently loses articles, which is far worse than one that under-merges.
    #[test]
    fn genuinely_different_urls_stay_different() {
        let distinct = [
            "https://example.com/a",
            "https://example.com/b",
            "http://example.com/a",
            "https://other.com/a",
            "https://example.com/a?id=7",
            "https://example.com/a?id=8",
            "https://example.com:8443/a",
        ];
        for (i, left) in distinct.iter().enumerate() {
            for right in &distinct[i + 1..] {
                assert_ne!(
                    canonicalize(left),
                    canonicalize(right),
                    "{left} and {right} collapsed"
                );
            }
        }
    }

    /// Nothing here may panic or refuse: a canonicalizer is on the ingest hot path
    /// and a `None` would drop the article.
    #[test]
    fn garbage_degrades_instead_of_failing() {
        assert_eq!(canonicalize(""), "");
        assert_eq!(canonicalize("not a url"), "not a url");
        // Same rubbish spelled two ways still deduplicates against itself.
        assert_eq!(canonicalize("  Not A URL  "), canonicalize("not   a url"));
        assert_eq!(canonicalize("https://"), "https://");
        assert_eq!(canonicalize("mailto:someone@example.com"), "mailto:someone@example.com");
        assert!(parse("https://").is_none());
        assert!(parse("/relative/path").is_none());
    }

    #[test]
    fn is_http_url_gates_what_a_route_will_store() {
        assert!(is_http_url("https://example.com/feed.xml"));
        assert!(is_http_url("HTTP://example.com/feed"));
        assert!(!is_http_url("ftp://example.com/feed"));
        assert!(!is_http_url("example.com/feed"));
        assert!(!is_http_url(""));
    }

    #[test]
    fn tracking_param_list_is_sorted_for_the_binary_search() {
        let mut sorted = TRACKING_PARAMS.to_vec();
        sorted.sort_unstable();
        assert_eq!(TRACKING_PARAMS, sorted.as_slice());
        assert!(is_tracking_param("UTM_Source"));
        assert!(is_tracking_param("ref"));
        assert!(!is_tracking_param("id"));
        assert!(!is_tracking_param("page"));
    }
}
