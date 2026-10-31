//! Burst detection — deciding what counts as "breaking" for a topic.
//!
//! A watch that fires is an interruption, so it has to be defensible. Everything here
//! is arithmetic over stored counts, and every alert carries the z-score and the
//! articles that caused it, so "why did this wake me" always has an answer.
//!
//! # Why the baseline is per hour-of-day
//!
//! News has a hard daily cycle: a topic that averages 4 articles/hour across a week
//! averages ~0 overnight and ~12 at 09:00. Compared against a FLAT weekly mean, every
//! weekday morning is a three-sigma event and the alert becomes an alarm clock. So the
//! baseline for 09:00 is built only from previous 09:00 buckets. That is also why a
//! week of history is the minimum useful window — it is seven samples per hour slot.
//!
//! # The two guards that stop a correct formula being useless
//!
//! - **[`MIN_STDEV`]** — a topic that normally sees zero articles has a standard
//!   deviation of zero, and `(1 - 0) / 0` is infinity. Every single article on a quiet
//!   topic would be a permanent maximum-confidence burst.
//! - **[`MIN_ABSOLUTE`]** — even with a floored stdev, going from 0 to 2 articles is
//!   statistically enormous and journalistically nothing. A burst needs to be a real
//!   volume of coverage, not just an unusual one.
//!
//! Both exist because the naive version of this test fires constantly on exactly the
//! topics a user cares most about — the narrow ones they set up deliberately.

/// Minimum standard deviation used in the z-score denominator.
pub const MIN_STDEV: f64 = 1.0;

/// Minimum article count in the hour before a burst can be declared, regardless of
/// how many standard deviations above baseline it is.
pub const MIN_ABSOLUTE: i64 = 4;

/// Default z-score at which a topic is called breaking.
pub const DEFAULT_Z_THRESHOLD: f64 = 3.0;

/// Hours a topic stays quiet after firing.
///
/// A developing story produces elevated volume for hours. Without a cooldown the same
/// event fires every hour it stays hot, which trains people to mute the app.
pub const COOLDOWN_HOURS: i64 = 6;

/// Minimum number of same-hour-of-day samples before a baseline is trusted.
///
/// Below this the app reports [`Verdict::NotEnoughHistory`] rather than guessing. A
/// mean and a standard deviation over two points are not a baseline, and a topic added
/// yesterday would otherwise fire on its first busy hour.
pub const MIN_BASELINE_SAMPLES: usize = 5;

/// Milliseconds in an hour.
const HOUR_MS: i64 = 3_600_000;

/// What the burst test concluded.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Volume is above the threshold and the cooldown has expired.
    Burst(Burst),
    /// Not enough same-hour samples to have a baseline at all.
    NotEnoughHistory { samples: usize },
    /// Above threshold, but the topic fired recently.
    Cooling { hours_remaining: i64 },
    /// Nothing unusual.
    Quiet(Burst),
}

/// The computed statistics of one hour, whether or not it fired.
///
/// Returned on the quiet path too, so the UI can chart "how close did that get"
/// without recomputing it differently somewhere else.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Burst {
    pub count: i64,
    pub z_score: f64,
    pub baseline_mean: f64,
    pub baseline_stdev: f64,
    pub hour_of_day: i64,
}

/// Mean and (population) standard deviation of `samples`.
///
/// Population rather than sample standard deviation: these are all the observations
/// there are for that hour slot, not a draw from a larger set.
#[must_use]
pub fn baseline(samples: &[i64]) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let n = samples.len() as f64;
    let mean = samples.iter().map(|c| *c as f64).sum::<f64>() / n;
    let variance = samples
        .iter()
        .map(|c| {
            let d = *c as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / n;
    (mean, variance.sqrt())
}

/// Decide whether `count` articles in this hour is a burst for the topic.
///
/// `same_hour_counts` are previous counts for the SAME hour-of-day (see the module
/// docs). `last_fired_at` is when this topic last raised a burst, if ever. `now` is
/// passed in rather than read so the whole test replays.
#[must_use]
pub fn evaluate(
    count: i64,
    same_hour_counts: &[i64],
    hour_of_day: i64,
    last_fired_at: Option<i64>,
    now: i64,
    z_threshold: f64,
) -> Verdict {
    if same_hour_counts.len() < MIN_BASELINE_SAMPLES {
        return Verdict::NotEnoughHistory {
            samples: same_hour_counts.len(),
        };
    }
    let (mean, stdev) = baseline(same_hour_counts);
    let z = (count as f64 - mean) / stdev.max(MIN_STDEV);
    let stats = Burst {
        count,
        z_score: z,
        baseline_mean: mean,
        baseline_stdev: stdev,
        hour_of_day,
    };

    if z < z_threshold || count < MIN_ABSOLUTE {
        return Verdict::Quiet(stats);
    }
    if let Some(fired) = last_fired_at {
        let elapsed = now - fired;
        let cooldown = COOLDOWN_HOURS * HOUR_MS;
        if elapsed < cooldown {
            // Round UP, so "1 hour remaining" never means "about to fire".
            let remaining = (cooldown - elapsed + HOUR_MS - 1) / HOUR_MS;
            return Verdict::Cooling {
                hours_remaining: remaining,
            };
        }
    }
    Verdict::Burst(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_786_348_800_000;
    /// A quiet weekday 09:00 slot: seven samples, all low.
    const QUIET_MORNINGS: &[i64] = &[1, 0, 2, 1, 1, 0, 2];
    /// A topic that is always busy at 09:00.
    const BUSY_MORNINGS: &[i64] = &[10, 12, 9, 11, 13, 10, 12];

    #[test]
    fn a_real_surge_on_a_quiet_topic_fires() {
        let verdict = evaluate(9, QUIET_MORNINGS, 9, None, NOW, DEFAULT_Z_THRESHOLD);
        match verdict {
            Verdict::Burst(b) => {
                assert_eq!(b.count, 9);
                assert!(b.z_score >= DEFAULT_Z_THRESHOLD);
            }
            other => panic!("expected a burst, got {other:?}"),
        }
    }

    #[test]
    fn an_ordinary_busy_morning_does_not_fire() {
        // THE case the hour-of-day baseline exists for. Twelve articles at 09:00 is
        // enormous against a flat weekly mean and completely normal against this
        // topic's own 09:00 history. A flat baseline turns the watch into an alarm
        // clock and the user mutes it.
        let verdict = evaluate(12, BUSY_MORNINGS, 9, None, NOW, DEFAULT_Z_THRESHOLD);
        assert!(matches!(verdict, Verdict::Quiet(_)), "got {verdict:?}");
    }

    #[test]
    fn a_zero_variance_topic_does_not_fire_on_a_single_article() {
        // Without MIN_STDEV this is a division by zero and every article on a silent
        // topic is an infinite-confidence burst, forever. Without MIN_ABSOLUTE it is
        // still a huge z-score on a journalistically meaningless jump.
        let never: &[i64] = &[0, 0, 0, 0, 0, 0, 0];
        let (_, stdev) = baseline(never);
        assert_eq!(stdev, 0.0, "the fixture must actually have zero variance");
        let verdict = evaluate(1, never, 3, None, NOW, DEFAULT_Z_THRESHOLD);
        assert!(matches!(verdict, Verdict::Quiet(_)), "got {verdict:?}");
    }

    #[test]
    fn the_absolute_floor_gates_a_statistically_huge_but_tiny_jump() {
        // 3 articles against a zero baseline clears z=3 but is below MIN_ABSOLUTE.
        let never: &[i64] = &[0, 0, 0, 0, 0, 0, 0];
        let verdict = evaluate(MIN_ABSOLUTE - 1, never, 3, None, NOW, DEFAULT_Z_THRESHOLD);
        assert!(matches!(verdict, Verdict::Quiet(_)), "got {verdict:?}");
        // One more article, and the same z-score now counts.
        let verdict = evaluate(MIN_ABSOLUTE, never, 3, None, NOW, DEFAULT_Z_THRESHOLD);
        assert!(matches!(verdict, Verdict::Burst(_)), "got {verdict:?}");
    }

    #[test]
    fn a_new_topic_reports_missing_history_rather_than_guessing() {
        let verdict = evaluate(50, &[1, 2], 9, None, NOW, DEFAULT_Z_THRESHOLD);
        assert_eq!(verdict, Verdict::NotEnoughHistory { samples: 2 });
    }

    #[test]
    fn a_developing_story_does_not_fire_every_hour() {
        let fired = NOW - 2 * HOUR_MS;
        let verdict = evaluate(9, QUIET_MORNINGS, 9, Some(fired), NOW, DEFAULT_Z_THRESHOLD);
        match verdict {
            Verdict::Cooling { hours_remaining } => {
                assert_eq!(hours_remaining, COOLDOWN_HOURS - 2);
            }
            other => panic!("expected cooling, got {other:?}"),
        }
    }

    #[test]
    fn the_cooldown_expires() {
        let fired = NOW - (COOLDOWN_HOURS + 1) * HOUR_MS;
        let verdict = evaluate(9, QUIET_MORNINGS, 9, Some(fired), NOW, DEFAULT_Z_THRESHOLD);
        assert!(matches!(verdict, Verdict::Burst(_)), "got {verdict:?}");
    }

    #[test]
    fn the_quiet_path_still_reports_the_statistics() {
        // So the UI can show "how close did that get" without a second, differently
        // written copy of this arithmetic.
        let verdict = evaluate(2, QUIET_MORNINGS, 9, None, NOW, DEFAULT_Z_THRESHOLD);
        match verdict {
            Verdict::Quiet(b) => {
                assert_eq!(b.count, 2);
                assert_eq!(b.hour_of_day, 9);
                assert!(b.baseline_mean > 0.0);
            }
            other => panic!("expected quiet stats, got {other:?}"),
        }
    }

    #[test]
    fn baseline_is_the_population_statistic() {
        let (mean, stdev) = baseline(&[2, 4, 4, 4, 5, 5, 7, 9]);
        assert!((mean - 5.0).abs() < 1e-12);
        // Population stdev of that classic set is exactly 2.
        assert!((stdev - 2.0).abs() < 1e-12, "got {stdev}");
    }

    #[test]
    fn evaluation_is_a_pure_function_of_its_inputs() {
        let a = evaluate(9, QUIET_MORNINGS, 9, None, NOW, DEFAULT_Z_THRESHOLD);
        let b = evaluate(9, QUIET_MORNINGS, 9, None, NOW, DEFAULT_Z_THRESHOLD);
        assert_eq!(a, b);
    }
}
