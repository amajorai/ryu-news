//! Tokenization and the one stopword list.
//!
//! # Why this module exists at all
//!
//! [`crate::models::HeadlineSnapshot`] ships `stopwords` over the KV seam to
//! `apps-store/news/hooks/ground.js` and documents it as "the exact stopword list the
//! sidecar used". That promise is only true if there is exactly one list in this
//! crate. A second `const STOPWORDS` in the clusterer would be the drift `models.rs`
//! warns about, discovered as a hook that quietly matches a different set of words
//! than the sidecar ranked.
//!
//! # Three tokenizers, named for their consumer, deliberately NOT unified
//!
//! They look similar enough that someone will eventually try to collapse them. Each
//! collapse breaks something specific:
//!
//! | fn | order/dups | stopwords | min length | consumer |
//! |----|-----------|-----------|-----------|----------|
//! | [`shingle_tokens`] | kept | kept | none | [`crate::simhash`] |
//! | [`query_tokens`] | kept | kept | none | [`crate::query`] |
//! | [`snapshot_tokens`] | deduped | dropped | 3 | the KV snapshot / `ground.js` |
//!
//! - **`query_tokens` must keep order and duplicates**, because a phrase matches a
//!   *contiguous run* of tokens — a set cannot express "export controls" adjacent.
//! - **`query_tokens` must not drop stopwords**, or `"war on drugs"` stops matching
//!   its own article, and must not impose a minimum length, or `AI`, `EU` and `US`
//!   — three of the most-watched terms in a news app — are erased from every query.
//! - **`snapshot_tokens` must do all three**, because it mirrors the tokenizer in
//!   `ground.js` byte-for-byte and that hook dedupes, filters and requires 3 chars.
//!   Nothing in this task's modules consumes it; its LIST is consumed by all of
//!   them, and defining the tokenizer next to the list is what makes the
//!   one-definition claim checkable.
//!
//! # No NFKC
//!
//! Neither `unicode-normalization` nor any crate that provides NFKC is in the root
//! `Cargo.lock`, and this app may not add one. Where the spec says NFKC, read
//! "lowercase + whitespace-normalize + trim" — see [`normalize`]. `str::to_lowercase`
//! is full Unicode case mapping (not ASCII), which covers the cases that actually
//! reach a news feed.

/// Words carried no meaning for matching, ranking or entity extraction.
///
/// **Sorted**, because [`is_stopword`] binary-searches it and a test asserts the
/// sort — an unsorted insert would make lookups silently miss.
///
/// It is a superset of the `FALLBACK_STOPWORDS` floor in `hooks/ground.js`: that
/// list is what an OLD sidecar's snapshot degrades to, this one is the truth. The
/// tail of it (`said`, `report`, `latest`, `news`) is news-register noise —
/// a headline verb that appears in a third of all headlines is not evidence that two
/// articles are the same story.
pub const STOPWORDS: &[&str] = &[
    "a",
    "about",
    "after",
    "again",
    "against",
    "all",
    "also",
    "am",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "back",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "cannot",
    "could",
    "did",
    "do",
    "does",
    "doing",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "get",
    "give",
    "had",
    "has",
    "have",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "itself",
    "just",
    "latest",
    "like",
    "make",
    "many",
    "me",
    "more",
    "most",
    "my",
    "myself",
    "new",
    "news",
    "no",
    "nor",
    "not",
    "now",
    "of",
    "off",
    "on",
    "once",
    "one",
    "only",
    "or",
    "other",
    "others",
    "ought",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "please",
    "report",
    "reports",
    "said",
    "same",
    "say",
    "says",
    "she",
    "should",
    "so",
    "some",
    "such",
    "take",
    "tell",
    "than",
    "thanks",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "us",
    "very",
    "was",
    "we",
    "were",
    "what",
    "whats",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "why",
    "will",
    "with",
    "would",
    "you",
    "your",
    "yours",
    "yourself",
];

/// Extra words that are never an entity even though they are always capitalized.
///
/// Separate from [`STOPWORDS`] rather than merged into it, because these must NOT be
/// dropped from a query or a shingle: `"January 6"` is a perfectly good watch and a
/// perfectly good shingle, it is just not the *name of something* that two outlets
/// covering the same story would share. Merging the two lists would quietly change
/// what [`query_tokens`] returns, which is the one tokenizer that must not filter.
///
/// Sorted, same as [`STOPWORDS`], and asserted so.
pub const ENTITY_STOPWORDS: &[&str] = &[
    "april",
    "august",
    "december",
    "february",
    "friday",
    "january",
    "july",
    "june",
    "march",
    "may",
    "monday",
    "november",
    "october",
    "saturday",
    "september",
    "sunday",
    "thursday",
    "tuesday",
    "wednesday",
];

/// Minimum token length in the snapshot tokenizer. Mirrors `MIN_TOKEN_LEN` in
/// `hooks/ground.js`; the two must move together or the hook's overlap test is run
/// against a token set the sidecar never produced.
pub const MIN_SNAPSHOT_TOKEN_LEN: usize = 3;

/// Is this word (already lowercased) a stopword?
pub fn is_stopword(word: &str) -> bool {
    STOPWORDS.binary_search(&word).is_ok()
}

/// Is this word (already lowercased) barred from being an entity? Stopwords plus
/// [`ENTITY_STOPWORDS`].
pub fn is_entity_stopword(word: &str) -> bool {
    is_stopword(word) || ENTITY_STOPWORDS.binary_search(&word).is_ok()
}

/// The stopword list as owned strings, for the KV snapshot's `stopwords` field.
pub fn stopword_list() -> Vec<String> {
    STOPWORDS.iter().map(|w| (*w).to_string()).collect()
}

/// Lowercase, collapse every whitespace run to one space, trim.
///
/// This is what "NFKC" means everywhere in this crate — see the module docs. Used for
/// the comparisons where two strings should be equal despite formatting: entity
/// names, a source's title, a phrase read out of a feed.
pub fn normalize(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut pending_space = false;
    for ch in lowered.chars() {
        if ch.is_whitespace() {
            // Only emit the space if something follows it, so the result is trimmed
            // without a second pass.
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

/// The one splitting rule: lowercase, then cut on anything that is not a letter or a
/// digit.
///
/// `char::is_alphanumeric` rather than an ASCII class, matching the `\p{L}\p{N}`
/// regex in `hooks/ground.js`: an article in a non-Latin script must tokenize into
/// words, not into one empty vector. Private, so the three public tokenizers stay
/// the only entry points and their differences stay visible at the call site.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
}

/// Tokens for SimHash shingling: every word, in order, duplicates kept.
///
/// No filtering at all. ALGORITHMS specifies the shingle input as "lowercased and
/// punctuation-stripped" and nothing else, and stopword removal would make two
/// articles that share only their function words look identical after shingling.
pub fn shingle_tokens(text: &str) -> Vec<String> {
    words(text).collect()
}

/// Tokens for the topic-query evaluator: every word, in order, duplicates kept.
///
/// Identical rule to [`shingle_tokens`] today and separate on purpose — see the
/// module docs for the three things that break if either one starts filtering.
pub fn query_tokens(text: &str) -> Vec<String> {
    words(text).collect()
}

/// Tokens for the KV snapshot: deduped in first-occurrence order, stopwords dropped,
/// shorter than [`MIN_SNAPSHOT_TOKEN_LEN`] dropped.
///
/// The dedupe preserves first-occurrence order rather than sorting, because
/// `ground.js` iterates the list and stops at the required overlap — an alphabetical
/// list would change which token it stops on, which is a behaviour difference for no
/// reason.
pub fn snapshot_tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in words(text) {
        if token.chars().count() < MIN_SNAPSHOT_TOKEN_LEN || is_stopword(&token) {
            continue;
        }
        if !out.iter().any(|seen| *seen == token) {
            out.push(token);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `is_stopword` binary-searches. An unsorted list does not fail to compile, it
    /// fails to find "the".
    #[test]
    fn the_word_lists_are_sorted_because_lookup_is_a_binary_search() {
        let mut sorted = STOPWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(STOPWORDS, sorted.as_slice(), "STOPWORDS is not sorted");
        let mut sorted_entities = ENTITY_STOPWORDS.to_vec();
        sorted_entities.sort_unstable();
        assert_eq!(ENTITY_STOPWORDS, sorted_entities.as_slice());
        assert!(is_stopword("the"));
        assert!(is_stopword("would"));
        assert!(!is_stopword("semiconductor"));
    }

    /// The whole reason for two lists: a month is not an entity but IS a legitimate
    /// query term and a legitimate shingle word.
    #[test]
    fn month_names_are_entity_stopwords_but_not_query_stopwords() {
        assert!(is_entity_stopword("january"));
        assert!(!is_stopword("january"));
        assert_eq!(
            query_tokens("January 6 hearings"),
            ["january", "6", "hearings"]
        );
    }

    #[test]
    fn normalize_lowercases_collapses_and_trims() {
        assert_eq!(
            normalize("  The\t Federal   Reserve\n"),
            "the federal reserve"
        );
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("   "), "");
        // Full Unicode case mapping, not ASCII: this is what stands in for NFKC.
        assert_eq!(normalize("ÉCONOMIE Française"), "économie française");
    }

    /// A phrase match needs adjacency, so the query tokenizer must not dedupe or
    /// reorder — this is the test that fails if someone "unifies" the tokenizers.
    #[test]
    fn query_tokens_keep_order_and_duplicates_and_short_words() {
        assert_eq!(
            query_tokens("Chips for chips: EU AI act"),
            ["chips", "for", "chips", "eu", "ai", "act"]
        );
        // A two-letter term survives. `snapshot_tokens` would drop all three.
        assert!(query_tokens("US EU AI").len() == 3);
    }

    #[test]
    fn snapshot_tokens_mirror_the_hooks_rule() {
        // "the" is a stopword, "of" is a stopword, "eu" is under the 3-char floor,
        // "chip" appears twice and is emitted once, in first-occurrence order.
        assert_eq!(
            snapshot_tokens("The EU chip act, and the chip subsidies of 2026"),
            ["chip", "act", "subsidies", "2026"]
        );
    }

    #[test]
    fn tokenizers_split_non_latin_scripts_into_words() {
        assert_eq!(query_tokens("日本 の 半導体"), ["日本", "の", "半導体"]);
        // Punctuation-only input is empty, not one empty token.
        assert!(query_tokens("—  … !!").is_empty());
    }
}
