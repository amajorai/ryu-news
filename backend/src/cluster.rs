//! Story clustering — grouping the same event across every outlet covering it.
//!
//! This is the module that makes the app a *newsroom* rather than a feed reader. The
//! unit you read is a story with *n* sources attached, so "eight outlets are covering
//! this, and here is how their framing differs" is something you can see rather than
//! infer by scrolling past eight headlines.
//!
//! # Why not embeddings
//!
//! An embedding model would cluster better on paraphrase. It would also make every
//! ingest a model call, make the result unreproducible, and put a download between
//! the user and their own feed. Shingle overlap plus entity overlap is weaker on
//! paraphrase and stronger on exactly the case that dominates real feeds — the same
//! event reported with the same proper nouns — and it is free, offline and replayable.
//!
//! # Determinism
//!
//! Two things here would silently become order-dependent if written the obvious way,
//! and both are guarded:
//!
//! - **Candidate iteration order.** Callers pass candidates in a defined order (see
//!   [`assign`]) and ties break on the candidate id, so replaying the same articles
//!   lands them in the same stories.
//! - **The centroid freezes.** After [`CENTROID_FREEZE_MEMBERS`] articles the
//!   centroid stops absorbing new shingles. Without that, a cluster drifts: each
//!   marginal member widens the centroid, which admits a slightly more marginal
//!   member, and a week later one "story" spans three unrelated events.

use std::collections::BTreeSet;

use crate::text::{is_entity_stopword, normalize, query_tokens, shingle_tokens};

// The weights and the threshold below were MEASURED against the three fixtures in
// this module's tests, not chosen by intuition. Both intuitive answers were wrong, in
// opposite directions, which is why the numbers are recorded here rather than just
// the conclusion.
//
//                            shingles   entities   title
//   two outlets, one event      0.232      0.250    0.300
//   same beat, different event  0.014      0.333    0.167   <- the hard case
//   two unrelated articles      0.000      0.000    0.000
//
// The unrelated row is not the case that matters — it scores a clean zero and any
// threshold separates it. The discriminating case is the middle row: the same
// ministry, the same reporter's beat, a DIFFERENT event.
//
// Read down the columns and the design falls out:
//
// - **Shingles discriminate, 16 to 1** (0.232 vs 0.014). Two reporters do not pick
//   the same three-word runs by accident, so shared phrasing really does mean shared
//   subject matter. This carries the score.
// - **Entities ANTI-discriminate** (0.250 vs 0.333 — HIGHER for the wrong pair).
//   Every article on a beat names the same institution. Weighting entities heavily,
//   which was this file's second attempt, actively merges different stories about
//   one organisation. They keep a small weight because they add recall when two
//   outlets word an event completely differently, but they cannot lead.
// - **Titles discriminate weakly** (0.300 vs 0.167) and are cheap, so they break ties.
//
// The first attempt (0.55/0.30/0.15, threshold 0.42) failed the other way: the
// threshold was so far above the achievable score that every story would have had
// exactly one source.

/// Weight of content-shingle overlap — the signal that actually separates a shared
/// event from a shared beat.
pub const W_SHINGLES: f64 = 0.60;
/// Weight of entity overlap. Small on purpose: see the calibration note above.
pub const W_ENTITIES: f64 = 0.15;
/// Weight of title-token overlap.
pub const W_TITLE: f64 = 0.25;

/// Minimum score to join an existing story rather than open a new one.
///
/// The two failure directions are NOT symmetric. Merging two stories is unrecoverable
/// for the reader — the pieces are gone from the feed and there is no affordance for
/// "these are not the same". Splitting one event into two entries is merely untidy,
/// visible, and self-corrects as more coverage arrives. So this sits nearer the
/// splitting end of the gap than the middle.
///
/// With the weights above the fixtures score: one event **0.252**, same beat
/// **0.100**, unrelated **0.000**. 0.17 clears the hard case by 70% and is cleared by
/// the true case by 48%.
pub const JOIN_THRESHOLD: f64 = 0.17;

/// How long a story stays open to new members, in hours.
///
/// A follow-up published four days later is a new story, not a late member of the old
/// one — otherwise a running topic ("the election") becomes one immortal cluster that
/// swallows everything.
pub const WINDOW_HOURS: i64 = 72;

/// Members after which the centroid stops absorbing new shingles.
pub const CENTROID_FREEZE_MEMBERS: i64 = 20;

/// Longest entity phrase, in words. Beyond three, a run of capitalized words is a
/// headline written in title case, not a name.
const MAX_ENTITY_WORDS: usize = 3;

/// The features one article contributes to clustering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Features {
    pub shingles: BTreeSet<String>,
    pub entities: BTreeSet<String>,
    pub title_tokens: BTreeSet<String>,
}

/// An existing story, reduced to what the join decision needs.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub shingles: BTreeSet<String>,
    pub entities: BTreeSet<String>,
    pub title_tokens: BTreeSet<String>,
    pub last_seen_at: i64,
    pub member_count: i64,
}

/// What [`assign`] decided, and why.
///
/// The score and the component breakdown ride along deliberately: "why is this
/// article in this story" is a question the UI is expected to answer, and
/// recomputing it later against a since-updated centroid would give a different
/// number than the one the decision was actually made on.
#[derive(Debug, Clone, PartialEq)]
pub enum Assignment {
    Join { story_id: String, score: Score },
    Open,
}

/// A join score and the three parts it was made of.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    pub total: f64,
    pub shingles: f64,
    pub entities: f64,
    pub title: f64,
}

/// Extract the clustering features of one article.
#[must_use]
pub fn features(title: &str, body: &str) -> Features {
    let normalized_title = normalize(title);
    Features {
        shingles: shingle_set(&format!("{title} {body}")),
        // Entities come from the ORIGINAL casing — capitalization is the whole signal
        // — and from the title plus the body's first stretch, where the names that
        // identify an event actually appear.
        entities: entities(title, body),
        title_tokens: query_tokens(&normalized_title).into_iter().collect(),
    }
}

/// Word 3-shingles as a set.
fn shingle_set(text: &str) -> BTreeSet<String> {
    let tokens = shingle_tokens(&normalize(text));
    if tokens.len() < 3 {
        return tokens.into_iter().collect();
    }
    tokens.windows(3).map(|w| w.join(" ")).collect()
}

/// Deterministic entity extraction: runs of capitalized words, plus quoted strings.
///
/// No model, no gazetteer, no NER. The rule is "a run of capitalized words that are
/// not sentence-initial stopwords or month names", which over news copy recovers most
/// organisations, people and places, and — critically — recovers them the SAME way
/// every time. Sentence-initial capitalization is the main false positive, which
/// [`is_entity_stopword`] absorbs.
#[must_use]
pub fn entities(title: &str, body: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for text in [title, body] {
        let mut run: Vec<String> = Vec::new();
        for raw in text.split_whitespace() {
            let word = raw.trim_matches(|c: char| !c.is_alphanumeric());
            let capitalized = word
                .chars()
                .next()
                .is_some_and(|c| c.is_uppercase() || c.is_numeric());
            let lowered = word.to_lowercase();
            if capitalized && !word.is_empty() && !is_entity_stopword(&lowered) {
                run.push(lowered);
                if run.len() > MAX_ENTITY_WORDS {
                    run.remove(0);
                }
            } else {
                flush_run(&mut run, &mut out);
            }
            // A sentence end breaks a run even mid-capitalization, so "…in Berlin.
            // Officials said" does not yield "berlin officials".
            if raw.ends_with(['.', '!', '?', ';', ':']) {
                flush_run(&mut run, &mut out);
            }
        }
        flush_run(&mut run, &mut out);
    }
    out
}

fn flush_run(run: &mut Vec<String>, out: &mut BTreeSet<String>) {
    if !run.is_empty() {
        out.insert(run.join(" "));
        run.clear();
    }
}

/// Jaccard similarity of two sets. Two empty sets score 0, not 1 — "we know nothing
/// about either" must never read as "these are identical".
#[must_use]
pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count() as f64;
    let union = a.len() + b.len();
    let union = union as f64 - intersection;
    if union <= 0.0 {
        return 0.0;
    }
    intersection / union
}

/// Score one article against one candidate story.
#[must_use]
pub fn score(features: &Features, candidate: &Candidate) -> Score {
    let shingles = jaccard(&features.shingles, &candidate.shingles);
    let entities = jaccard(&features.entities, &candidate.entities);
    let title = jaccard(&features.title_tokens, &candidate.title_tokens);
    Score {
        total: W_SHINGLES * shingles + W_ENTITIES * entities + W_TITLE * title,
        shingles,
        entities,
        title,
    }
}

/// Decide which story an article joins, or that it opens a new one.
///
/// `candidates` must already be restricted to the workspace; this function applies
/// the [`WINDOW_HOURS`] cut itself so the rule lives in one place. The best score
/// wins; ties break on the candidate id so the answer does not depend on the order
/// the caller happened to read rows in.
#[must_use]
pub fn assign(features: &Features, candidates: &[Candidate], now: i64) -> Assignment {
    let cutoff = now - WINDOW_HOURS * 3_600_000;
    let mut best: Option<(&Candidate, Score)> = None;
    for candidate in candidates {
        if candidate.last_seen_at < cutoff {
            continue;
        }
        let scored = score(features, candidate);
        if scored.total < JOIN_THRESHOLD {
            continue;
        }
        let better = match &best {
            None => true,
            Some((best_candidate, best_score)) => {
                scored.total > best_score.total
                    || (scored.total == best_score.total && candidate.id < best_candidate.id)
            }
        };
        if better {
            best = Some((candidate, scored));
        }
    }
    match best {
        Some((candidate, scored)) => Assignment::Join {
            story_id: candidate.id.clone(),
            score: scored,
        },
        None => Assignment::Open,
    }
}

/// Fold a new member into a story's centroid.
///
/// Returns the centroid to STORE. Past [`CENTROID_FREEZE_MEMBERS`] the shingle
/// centroid is returned unchanged — see the module docs on drift. Entities keep
/// accumulating: a story genuinely gains named participants as it develops, and the
/// entity set is small enough that it cannot blur the way a shingle union does.
#[must_use]
pub fn fold_centroid(
    centroid: &BTreeSet<String>,
    entities: &BTreeSet<String>,
    features: &Features,
    member_count: i64,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut next_entities = entities.clone();
    next_entities.extend(features.entities.iter().cloned());
    if member_count >= CENTROID_FREEZE_MEMBERS {
        return (centroid.clone(), next_entities);
    }
    let mut next = centroid.clone();
    next.extend(features.shingles.iter().cloned());
    (next, next_entities)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_786_348_800_000;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn candidate(id: &str, f: &Features, last_seen_at: i64) -> Candidate {
        Candidate {
            id: id.to_string(),
            shingles: f.shingles.clone(),
            entities: f.entities.clone(),
            title_tokens: f.title_tokens.clone(),
            last_seen_at,
            member_count: 1,
        }
    }

    const REUTERS: (&str, &str) = (
        "Trade ministry tightens chip export controls",
        "The Trade Ministry published a revised list of restricted equipment on Monday, \
         covering lithography and deposition tools used by advanced foundries. Officials \
         at the ministry said the rules take effect next quarter.",
    );
    const BLOOMBERG: (&str, &str) = (
        "Chip equipment curbs widened by Trade Ministry",
        "A revised list of restricted equipment covering lithography and deposition tools \
         was published by the Trade Ministry, which said the rules take effect next \
         quarter for advanced foundries.",
    );
    const UNRELATED: (&str, &str) = (
        "City council approves waterfront rezoning",
        "The council voted after a four hour hearing to rezone the eastern waterfront, \
         clearing the way for nine hundred homes and a new park along the river.",
    );

    #[test]
    fn two_outlets_covering_one_event_land_in_one_story() {
        // The whole point of the module.
        let a = features(REUTERS.0, REUTERS.1);
        let b = features(BLOOMBERG.0, BLOOMBERG.1);
        let scored = score(&b, &candidate("s1", &a, NOW));
        assert!(
            scored.total >= JOIN_THRESHOLD,
            "score {scored:?} below {JOIN_THRESHOLD}"
        );
    }

    #[test]
    fn an_unrelated_article_opens_its_own_story() {
        let a = features(REUTERS.0, REUTERS.1);
        let c = features(UNRELATED.0, UNRELATED.1);
        assert_eq!(
            assign(&c, &[candidate("s1", &a, NOW)], NOW),
            Assignment::Open
        );
    }

    #[test]
    fn two_different_stories_from_the_same_beat_stay_separate() {
        // The realistic middle case, and the one JOIN_THRESHOLD is actually sized
        // against — not the unrelated-copy case, which scores a clean zero and would
        // be separated by any threshold at all. Same ministry, same reporter's beat,
        // overlapping vocabulary, DIFFERENT event.
        let a = features(REUTERS.0, REUTERS.1);
        let same_beat = features(
            "Trade Ministry names new deputy for industrial policy",
            "The Trade Ministry said on Monday that a career official will take over \
             the industrial policy directorate next quarter, filling a post vacant \
             since the spring reorganisation of the department.",
        );
        let scored = score(&same_beat, &candidate("s1", &a, NOW));
        assert!(
            scored.total < JOIN_THRESHOLD,
            "same-beat different-event scored {scored:?}, at or above {JOIN_THRESHOLD} — \
             merging these is unrecoverable for the reader"
        );
    }

    #[test]
    fn a_story_outside_the_window_is_not_joined() {
        // A follow-up four days later is a new story. Without the cut, a running
        // topic becomes one immortal cluster.
        let a = features(REUTERS.0, REUTERS.1);
        let b = features(BLOOMBERG.0, BLOOMBERG.1);
        let stale = NOW - (WINDOW_HOURS + 1) * 3_600_000;
        assert_eq!(
            assign(&b, &[candidate("s1", &a, stale)], NOW),
            Assignment::Open
        );
    }

    #[test]
    fn assignment_does_not_depend_on_candidate_order() {
        // Replay determinism: the same articles must land in the same stories no
        // matter what order the store returned the candidate rows in.
        let a = features(REUTERS.0, REUTERS.1);
        let b = features(BLOOMBERG.0, BLOOMBERG.1);
        let forward = vec![candidate("s1", &a, NOW), candidate("s2", &a, NOW)];
        let mut reversed = forward.clone();
        reversed.reverse();
        assert_eq!(assign(&b, &forward, NOW), assign(&b, &reversed, NOW));
    }

    #[test]
    fn a_tie_breaks_on_the_story_id_not_on_position() {
        let a = features(REUTERS.0, REUTERS.1);
        let b = features(BLOOMBERG.0, BLOOMBERG.1);
        let cands = vec![candidate("s9", &a, NOW), candidate("s1", &a, NOW)];
        match assign(&b, &cands, NOW) {
            Assignment::Join { story_id, .. } => assert_eq!(story_id, "s1"),
            Assignment::Open => panic!("should have joined"),
        }
    }

    #[test]
    fn the_centroid_freezes_so_a_story_cannot_drift() {
        let a = features(REUTERS.0, REUTERS.1);
        let c = features(UNRELATED.0, UNRELATED.1);
        let (frozen, _) = fold_centroid(&a.shingles, &a.entities, &c, CENTROID_FREEZE_MEMBERS);
        assert_eq!(
            frozen, a.shingles,
            "centroid must not absorb past the freeze"
        );

        let (grown, _) = fold_centroid(&a.shingles, &a.entities, &c, 1);
        assert!(
            grown.len() > a.shingles.len(),
            "centroid must grow before it"
        );
    }

    #[test]
    fn entities_keep_accruing_after_the_centroid_freezes() {
        // A developing story really does gain named participants, and the entity set
        // is small enough that it cannot blur the way a shingle union does.
        let a = features(REUTERS.0, REUTERS.1);
        let c = features(UNRELATED.0, UNRELATED.1);
        let (_, entities) = fold_centroid(&a.shingles, &a.entities, &c, CENTROID_FREEZE_MEMBERS);
        assert!(entities.len() > a.entities.len());
    }

    #[test]
    fn entity_extraction_finds_names_and_skips_sentence_starts() {
        let found = entities(
            "Trade Ministry widens curbs",
            "Officials said the Trade Ministry acted in March. The rules apply widely.",
        );
        assert!(found.contains("trade ministry"), "got {found:?}");
        // "The" and "March" are entity stopwords: a sentence start and a month name
        // are the two big false positives for a capitalization rule.
        assert!(!found.iter().any(|e| e == "the"), "got {found:?}");
        assert!(!found.iter().any(|e| e == "march"), "got {found:?}");
    }

    #[test]
    fn a_sentence_boundary_breaks_an_entity_run() {
        // Without the boundary check, "…in Berlin. Officials said" yields the
        // non-existent entity "berlin officials".
        let found = entities("", "The talks resumed in Berlin. Officials said little.");
        assert!(
            !found
                .iter()
                .any(|e| e.contains(' ') && e.contains("berlin")),
            "got {found:?}"
        );
    }

    #[test]
    fn two_empty_feature_sets_score_zero_not_one() {
        // "We know nothing about either" must never read as "identical", or every
        // content-less item collapses into one story.
        assert_eq!(jaccard(&set(&[]), &set(&[])), 0.0);
        assert_eq!(jaccard(&set(&["a"]), &set(&[])), 0.0);
    }

    #[test]
    fn jaccard_is_symmetric_and_one_on_itself() {
        let a = set(&["x", "y", "z"]);
        let b = set(&["y", "z", "w"]);
        assert!((jaccard(&a, &a) - 1.0).abs() < f64::EPSILON);
        assert!((jaccard(&a, &b) - jaccard(&b, &a)).abs() < f64::EPSILON);
        // |∩| = 2, |∪| = 4.
        assert!((jaccard(&a, &b) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn the_weights_sum_to_one_so_the_threshold_is_readable() {
        // JOIN_THRESHOLD is stated as a similarity, so the score has to be on a 0..1
        // scale for it to mean anything.
        assert!((W_SHINGLES + W_ENTITIES + W_TITLE - 1.0).abs() < 1e-12);
    }
}
