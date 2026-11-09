//! SimHash over word shingles — dedupe layer 2.
//!
//! Layer 1 ([`crate::canon`]) collapses two links that are literally the same page.
//! This layer collapses two pages that are the same *story text* under different
//! URLs: syndicated wire copy running on six sites, a publisher's AMP and canonical
//! versions, an article re-posted under a new headline. That is the single biggest
//! source of feed noise, and no amount of URL normalization touches it.
//!
//! # Why SimHash rather than a similarity score
//!
//! Comparing every new article against every stored one is quadratic, and at a few
//! thousand articles a day that is the whole tick budget. SimHash turns "are these
//! texts similar" into "do these two 64-bit integers differ in at most `k` bits",
//! and the [banded index](`band_keys`) turns *that* into a handful of hash probes.
//! The cost is that it is a heuristic — see [`NEAR_DUPLICATE_DISTANCE`].
//!
//! # Determinism
//!
//! Every step here is a pure function of the input text: a fixed FNV-1a hash (not
//! `DefaultHasher`, whose output is randomly seeded per process and would make the
//! same article hash differently on every boot), a fixed shingle width, and integer
//! arithmetic throughout. The same text yields the same fingerprint forever, on any
//! machine — which matters because these values are STORED and compared against
//! articles fingerprinted by an earlier build.

use crate::text::shingle_tokens;

/// Words per shingle.
///
/// Three is the usual choice and the reason is worth stating: single words make
/// every article about the same subject look identical, and five-word shingles are
/// so specific that a one-word edit in a re-post breaks every shingle that covers
/// it. Three tolerates small edits while still keying on phrasing rather than topic.
pub const SHINGLE_WIDTH: usize = 3;

/// Maximum Hamming distance at which two fingerprints are called near-duplicates.
///
/// Three out of 64 bits. This is a threshold on a heuristic, so it is a trade rather
/// than a truth: raising it starts collapsing two genuinely different articles that
/// happen to share boilerplate, and lowering it starts letting the same wire story
/// through twice because one site appended a stock ticker. Three is the standard
/// operating point for 64-bit SimHash over documents, and it is also the largest
/// value the [banded index](`band_keys`) can find exhaustively: with 4 bands, a pair
/// differing in ≤3 bits must agree exactly on at least one band (pigeonhole), and a
/// 4-bit difference can be spread one-per-band and be missed.
pub const NEAR_DUPLICATE_DISTANCE: u32 = 3;

/// Number of bands the fingerprint is split into for indexed lookup.
pub const BANDS: usize = 4;

/// Bits per band.
pub const BAND_BITS: u32 = 64 / BANDS as u32;

/// FNV-1a, 64-bit.
///
/// Deliberately NOT `std::collections::hash_map::DefaultHasher`: that is SipHash with
/// a per-process random seed, so it would fingerprint the same article differently
/// after every restart and every stored `simhash` would become meaningless. A stored
/// fingerprint has to mean the same thing next week as it did today, so the hash has
/// to be specified rather than borrowed.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The 64-bit SimHash of `text`.
///
/// Empty or near-empty input yields `0`, which callers must treat as "no fingerprint"
/// rather than as a fingerprint: every empty article would otherwise be a duplicate
/// of every other empty article. [`is_near_duplicate`] enforces that.
#[must_use]
pub fn simhash(text: &str) -> u64 {
    let tokens = shingle_tokens(text);
    if tokens.len() < SHINGLE_WIDTH {
        // Too short to shingle. Fall back to hashing the tokens individually rather
        // than returning 0, so a three-word headline still gets a real fingerprint.
        if tokens.is_empty() {
            return 0;
        }
        return fold(tokens.iter().map(|t| fnv1a(t.as_bytes())));
    }
    fold(
        tokens
            .windows(SHINGLE_WIDTH)
            .map(|window| fnv1a(window.join(" ").as_bytes())),
    )
}

/// The SimHash fold: sum each bit position as +1/-1 across every shingle hash, then
/// keep the sign.
fn fold(hashes: impl Iterator<Item = u64>) -> u64 {
    let mut counts = [0i64; 64];
    let mut seen = 0u64;
    for hash in hashes {
        seen += 1;
        for (bit, count) in counts.iter_mut().enumerate() {
            if hash >> bit & 1 == 1 {
                *count += 1;
            } else {
                *count -= 1;
            }
        }
    }
    if seen == 0 {
        return 0;
    }
    let mut out = 0u64;
    for (bit, count) in counts.iter().enumerate() {
        // A tie (count == 0) resolves to 0, deterministically. It can only happen
        // with an even shingle count, and picking a side arbitrarily is fine as long
        // as the same input always picks the SAME side.
        if *count > 0 {
            out |= 1 << bit;
        }
    }
    out
}

/// Hamming distance between two fingerprints.
#[must_use]
pub fn distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Whether two fingerprints are near-duplicates.
///
/// A zero fingerprint is never a duplicate of anything, including another zero: `0`
/// means "there was not enough text to fingerprint", and treating it as a value would
/// collapse every content-less article into one.
#[must_use]
pub fn is_near_duplicate(a: u64, b: u64) -> bool {
    a != 0 && b != 0 && distance(a, b) <= NEAR_DUPLICATE_DISTANCE
}

/// The band keys of a fingerprint: `BANDS` values of `BAND_BITS` bits each, tagged
/// with their band index.
///
/// Tagging matters. Without the index in the key, the same 16-bit pattern appearing
/// in band 0 of one article and band 3 of another would collide in the lookup table
/// and produce candidates that share no bits in the same positions at all.
///
/// # The pigeonhole argument
///
/// Two fingerprints within [`NEAR_DUPLICATE_DISTANCE`] (3) bits differ in at most 3
/// of the 4 bands, so at least one band is IDENTICAL. Probing all four band keys and
/// then checking the real distance on the candidates therefore finds every true
/// near-duplicate — the index is an exact accelerator here, not an approximation.
/// This is why [`NEAR_DUPLICATE_DISTANCE`] cannot be raised past `BANDS - 1` without
/// also changing `BANDS`.
#[must_use]
pub fn band_keys(fingerprint: u64) -> [u64; BANDS] {
    let mut keys = [0u64; BANDS];
    let mask = (1u64 << BAND_BITS) - 1;
    for (index, key) in keys.iter_mut().enumerate() {
        let band = fingerprint >> (index as u32 * BAND_BITS) & mask;
        // Tag with the band index in the high bits so bands cannot collide.
        *key = band | (index as u64) << BAND_BITS;
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wire story at roughly the length real ones arrive at.
    ///
    /// The length is not incidental to the test. Hamming distance over a fixed 64-bit
    /// fingerprint is a proportion problem: appending three words to a 30-word stub
    /// rewrites an eighth of its shingles and moves ~8 bits, while appending the same
    /// three words to a 150-word story moves one or two. A short fixture would make
    /// this module look broken when it is behaving exactly as designed — see
    /// `a_short_headline_pair_is_below_what_this_layer_can_resolve` for the honest
    /// statement of where that leaves short items.
    const WIRE: &str = "Chip and wafer export controls tighten as the trade ministry \
        publishes a revised list of restricted equipment covering lithography and \
        deposition tools used by advanced foundries. The ministry said the updated \
        list takes effect at the start of next quarter and applies to shipments \
        already under contract, a change from the previous guidance which had \
        exempted orders placed before the announcement. Industry groups representing \
        equipment vendors warned that the retroactive element would force them to \
        renegotiate agreements signed as long as eighteen months ago. Two officials \
        briefed on the drafting said the exemption was removed late in the process \
        after concerns that it created an obvious route around the restrictions. The \
        ministry declined to say how many pending shipments are affected, though it \
        confirmed that licences already granted will be honoured. Analysts expect the \
        immediate impact to fall on suppliers of deposition and etch systems rather \
        than on lithography, where the existing controls were already comprehensive.";

    #[test]
    fn the_same_text_always_fingerprints_the_same_way() {
        // The whole stored-fingerprint scheme rests on this. If it ever fails, the
        // hash has picked up a per-process seed and every historical `simhash`
        // column is silently meaningless.
        assert_eq!(simhash(WIRE), simhash(WIRE));
        assert_eq!(simhash("a b c d"), simhash("a b c d"));
    }

    #[test]
    fn punctuation_and_case_do_not_change_the_fingerprint() {
        assert_eq!(simhash("Hello, World! Again"), simhash("hello world again"));
    }

    #[test]
    fn a_syndicated_repost_with_a_small_edit_is_a_near_duplicate() {
        // The case this whole module exists for: identical wire copy, one site has
        // appended a boilerplate sentence and changed the headline wording.
        let a = simhash(WIRE);
        let b = simhash(&format!("{WIRE} Reporting by staff."));
        assert!(
            is_near_duplicate(a, b),
            "distance was {} (limit {NEAR_DUPLICATE_DISTANCE})",
            distance(a, b)
        );
    }

    #[test]
    fn two_unrelated_articles_are_not_near_duplicates() {
        let a = simhash(WIRE);
        let b = simhash(
            "The city council approved a rezoning plan for the eastern waterfront \
             after a four hour hearing, clearing the way for nine hundred homes",
        );
        assert!(!is_near_duplicate(a, b), "distance was {}", distance(a, b));
    }

    #[test]
    fn a_short_headline_pair_is_below_what_this_layer_can_resolve() {
        // The honest limit, asserted so nobody discovers it as a surprise in
        // production. Two headline-only items that differ by one word are NOT caught
        // here: over a handful of shingles a single edit moves far more than three of
        // the 64 bits, and the threshold cannot be raised past 3 without breaking the
        // pigeonhole property the banded index depends on.
        //
        // This is a real gap, and it is the CLUSTERER's job, not this layer's — a
        // story clustered from title and entity overlap catches what a content
        // fingerprint over near-zero content cannot. Dedupe is layered for exactly
        // this reason; do not "fix" it by loosening the threshold.
        let a = simhash("budget passes after long debate");
        let b = simhash("budget passes after lengthy debate");
        assert!(distance(a, b) > NEAR_DUPLICATE_DISTANCE);
    }

    #[test]
    fn an_empty_fingerprint_is_never_a_duplicate_even_of_another_empty_one() {
        assert_eq!(simhash(""), 0);
        assert!(!is_near_duplicate(0, 0));
        assert!(!is_near_duplicate(0, simhash(WIRE)));
    }

    #[test]
    fn a_headline_shorter_than_the_shingle_width_still_fingerprints() {
        // Two words cannot form a 3-shingle; falling through to 0 would make every
        // short item a non-fingerprint and defeat dedupe on headline-only feeds.
        let short = simhash("budget passes");
        assert_ne!(short, 0);
        assert_eq!(short, simhash("Budget passes!"));
    }

    #[test]
    fn near_duplicates_always_share_at_least_one_band() {
        // The pigeonhole property the banded lookup depends on. If this fails, the
        // index silently stops finding real duplicates.
        let a = simhash(WIRE);
        for flip in 0..NEAR_DUPLICATE_DISTANCE {
            let b = a ^ ((1u64 << flip) | (1u64 << (flip + 20)) | (1u64 << (flip + 40)));
            assert!(distance(a, b) <= NEAR_DUPLICATE_DISTANCE);
            let (ka, kb) = (band_keys(a), band_keys(b));
            assert!(
                ka.iter().zip(kb.iter()).any(|(x, y)| x == y),
                "no shared band for a 3-bit difference"
            );
        }
    }

    #[test]
    fn band_keys_from_different_bands_never_collide() {
        // Same 16-bit pattern in every band; the tag must keep the four keys distinct.
        let repeated = 0x00ff_00ff_00ff_00ffu64;
        let keys = band_keys(repeated);
        for i in 0..BANDS {
            for j in (i + 1)..BANDS {
                assert_ne!(keys[i], keys[j], "band {i} and {j} collided");
            }
        }
    }

    #[test]
    fn distance_is_symmetric_and_zero_on_itself() {
        let a = simhash(WIRE);
        let b = simhash("something else entirely, with different words");
        assert_eq!(distance(a, a), 0);
        assert_eq!(distance(a, b), distance(b, a));
    }
}
