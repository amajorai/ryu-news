//! Feed ranking — and the reason each item is where it is.
//!
//! Every ranked feed a person uses is opaque, and the result is that nobody trusts
//! any of them. The whole design here is that the score is a product of four named
//! factors, each of which is returned alongside it, so the UI can answer "why is this
//! at the top" with the actual arithmetic rather than a description of it.
//!
//! That constraint is what rules out the obvious alternatives. A learned ranker or a
//! hand-tuned additive model with a dozen features would very likely order the feed
//! better; neither can be explained in a tooltip, and neither replays.
//!
//! # Why a product rather than a sum
//!
//! A weighted sum lets one big factor carry an item on its own: a story with forty
//! sources stays near the top for days regardless of age. A product means every factor
//! is a veto — old enough is buried no matter how well covered it is — which is what
//! "recency-first with corrections" actually means.

/// Default half-life of the recency term, in hours.
///
/// Twelve hours means this morning's news outranks yesterday evening's by ~3×, and
/// yesterday morning's is down 4×. Short enough that the feed turns over daily,
/// long enough that something published overnight is still visible at breakfast.
pub const DEFAULT_HALF_LIFE_HOURS: f64 = 12.0;

/// Multiplier applied to an unread item.
///
/// Deliberately small. A large unread bonus turns the feed into a to-do list that
/// refuses to move on, which is the failure mode of every "smart inbox".
pub const UNREAD_BONUS: f64 = 1.25;

/// Cap on the source-count term's input.
///
/// Coverage saturates: the difference between one outlet and six is the whole signal,
/// and the difference between forty and eighty is noise from syndication that
/// [`crate::simhash`] did not catch. Without a cap, one over-syndicated wire story
/// pins itself to the top of the feed for a day.
pub const MAX_SOURCE_COUNT: f64 = 12.0;

/// One item's score and the factors that produced it.
///
/// The factors are not diagnostics — they are part of the contract. `total` is exactly
/// their product, which [`Factors::verify`] asserts in tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Factors {
    pub total: f64,
    pub recency: f64,
    pub coverage: f64,
    pub topic: f64,
    pub unread: f64,
}

impl Factors {
    /// Whether `total` really is the product of the parts, within float tolerance.
    #[must_use]
    pub fn verify(&self) -> bool {
        let product = self.recency * self.coverage * self.topic * self.unread;
        (self.total - product).abs() < 1e-9
    }
}

/// What ranking needs to know about an article. Deliberately not the full model type:
/// this module must stay a pure function, and taking the row would invite reading a
/// clock or a store out of it.
#[derive(Debug, Clone, Copy)]
pub struct Input {
    pub published_at: i64,
    /// How many distinct outlets cover this article's story. 1 for an unclustered item.
    pub source_count: i64,
    /// How many saved topics matched it.
    pub topic_matches: i64,
    pub is_read: bool,
}

/// Score one article. `now` is passed in so a feed replays identically.
#[must_use]
pub fn score(input: &Input, now: i64, half_life_hours: f64) -> Factors {
    let age_hours = ((now - input.published_at).max(0) as f64) / 3_600_000.0;
    let half_life = if half_life_hours > 0.0 {
        half_life_hours
    } else {
        DEFAULT_HALF_LIFE_HOURS
    };
    let recency = 0.5f64.powf(age_hours / half_life);

    // ln(1) = 0, so a single-source item gets a coverage factor of exactly 1 and is
    // neither rewarded nor punished. Logarithmic because the step from 1 outlet to 2
    // is meaningful and the step from 20 to 21 is not.
    let sources = (input.source_count.max(1) as f64).min(MAX_SOURCE_COUNT);
    let coverage = 1.0 + sources.ln();

    // Same shape, same reason: matching a second topic says something, matching a
    // ninth says the user writes broad topics.
    let topic = 1.0 + (input.topic_matches.max(0) as f64 + 1.0).ln();

    let unread = if input.is_read { 1.0 } else { UNREAD_BONUS };

    Factors {
        total: recency * coverage * topic * unread,
        recency,
        coverage,
        topic,
        unread,
    }
}

/// Rank a set of articles, highest first.
///
/// Ties break on `published_at` descending and then on the caller-supplied id, so the
/// order is total and stable — an unstable sort over equal scores would reshuffle the
/// feed on every refresh for no reason the user could see.
#[must_use]
pub fn rank<T: Clone>(
    items: &[(String, Input, T)],
    now: i64,
    half_life_hours: f64,
) -> Vec<(String, Factors, T)> {
    let mut scored: Vec<(String, Factors, T)> = items
        .iter()
        .map(|(id, input, payload)| (id.clone(), score(input, now, half_life_hours), payload.clone()))
        .collect();
    scored.sort_by(|a, b| {
        b.1.total
            .partial_cmp(&a.1.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_786_348_800_000;
    const HOUR: i64 = 3_600_000;

    fn input(age_hours: i64, sources: i64, topics: i64, read: bool) -> Input {
        Input {
            published_at: NOW - age_hours * HOUR,
            source_count: sources,
            topic_matches: topics,
            is_read: read,
        }
    }

    #[test]
    fn the_total_is_exactly_the_product_of_the_reported_factors() {
        // The contract the whole "why is this here" affordance rests on. If the total
        // and the factors ever drift apart, the UI explains the ranking with numbers
        // that did not produce it.
        for age in [0, 1, 6, 24, 72] {
            for sources in [1, 3, 40] {
                for topics in [0, 1, 5] {
                    for read in [false, true] {
                        let f = score(&input(age, sources, topics, read), NOW, DEFAULT_HALF_LIFE_HOURS);
                        assert!(f.verify(), "factors do not multiply to total: {f:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn recency_halves_on_the_half_life() {
        let fresh = score(&input(0, 1, 0, false), NOW, DEFAULT_HALF_LIFE_HOURS);
        let old = score(&input(12, 1, 0, false), NOW, DEFAULT_HALF_LIFE_HOURS);
        assert!((fresh.recency - 1.0).abs() < 1e-12);
        assert!((old.recency - 0.5).abs() < 1e-12);
    }

    #[test]
    fn a_single_source_item_is_neither_rewarded_nor_punished() {
        let f = score(&input(0, 1, 0, true), NOW, DEFAULT_HALF_LIFE_HOURS);
        assert!((f.coverage - 1.0).abs() < 1e-12);
    }

    #[test]
    fn broad_coverage_lifts_an_item_but_cannot_outrank_a_day_of_age() {
        // The product-not-sum property. A story on forty outlets from yesterday must
        // not sit above a fresh single-source item, or the feed stops being a feed.
        let well_covered_but_old = score(&input(24, 40, 0, false), NOW, DEFAULT_HALF_LIFE_HOURS);
        let fresh_and_alone = score(&input(0, 1, 0, false), NOW, DEFAULT_HALF_LIFE_HOURS);
        assert!(fresh_and_alone.total > well_covered_but_old.total);
    }

    #[test]
    fn source_count_saturates_so_syndication_cannot_pin_an_item() {
        let twelve = score(&input(0, 12, 0, false), NOW, DEFAULT_HALF_LIFE_HOURS);
        let eighty = score(&input(0, 80, 0, false), NOW, DEFAULT_HALF_LIFE_HOURS);
        assert!((twelve.coverage - eighty.coverage).abs() < 1e-12);
    }

    #[test]
    fn reading_an_item_lowers_it_without_burying_it() {
        let unread = score(&input(3, 2, 1, false), NOW, DEFAULT_HALF_LIFE_HOURS);
        let read = score(&input(3, 2, 1, true), NOW, DEFAULT_HALF_LIFE_HOURS);
        assert!(unread.total > read.total);
        // The bonus is deliberately mild: a read item must still beat one four times
        // older, or the feed becomes a to-do list.
        let much_older_unread = score(&input(30, 2, 1, false), NOW, DEFAULT_HALF_LIFE_HOURS);
        assert!(read.total > much_older_unread.total);
    }

    #[test]
    fn an_item_published_in_the_future_is_not_boosted_above_a_fresh_one() {
        // Feeds really do carry future timestamps, from timezone bugs and from
        // scheduled posts. `age.max(0)` means the worst case is a tie with "now",
        // never an unbounded boost that pins the item to the top forever.
        let future = score(&input(-48, 1, 0, false), NOW, DEFAULT_HALF_LIFE_HOURS);
        let now_item = score(&input(0, 1, 0, false), NOW, DEFAULT_HALF_LIFE_HOURS);
        assert!((future.total - now_item.total).abs() < 1e-12);
    }

    #[test]
    fn a_zero_half_life_falls_back_rather_than_dividing_by_zero() {
        let f = score(&input(6, 1, 0, false), NOW, 0.0);
        let default = score(&input(6, 1, 0, false), NOW, DEFAULT_HALF_LIFE_HOURS);
        assert!((f.total - default.total).abs() < 1e-12);
    }

    #[test]
    fn ranking_is_stable_and_total_across_equal_scores() {
        // Identical inputs under different ids must come back in a fixed order, or
        // the feed reshuffles on every refresh for no visible reason.
        let items: Vec<(String, Input, ())> = vec![
            ("c".into(), input(1, 1, 0, false), ()),
            ("a".into(), input(1, 1, 0, false), ()),
            ("b".into(), input(1, 1, 0, false), ()),
        ];
        let ordered: Vec<String> = rank(&items, NOW, DEFAULT_HALF_LIFE_HOURS)
            .into_iter()
            .map(|(id, _, ())| id)
            .collect();
        assert_eq!(ordered, vec!["a", "b", "c"]);

        let mut shuffled = items;
        shuffled.reverse();
        let reordered: Vec<String> = rank(&shuffled, NOW, DEFAULT_HALF_LIFE_HOURS)
            .into_iter()
            .map(|(id, _, ())| id)
            .collect();
        assert_eq!(reordered, vec!["a", "b", "c"]);
    }

    #[test]
    fn ranking_puts_the_highest_score_first() {
        let items: Vec<(String, Input, ())> = vec![
            ("old".into(), input(48, 1, 0, true), ()),
            ("hot".into(), input(0, 6, 2, false), ()),
            ("mid".into(), input(6, 2, 0, false), ()),
        ];
        let ordered: Vec<String> = rank(&items, NOW, DEFAULT_HALF_LIFE_HOURS)
            .into_iter()
            .map(|(id, _, ())| id)
            .collect();
        assert_eq!(ordered, vec!["hot", "mid", "old"]);
    }
}
