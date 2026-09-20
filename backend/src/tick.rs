//! The poll loop: ingest, cluster, match, detect bursts, and write the daily brief.
//!
//! Everything is best-effort. A pass that fails must leave the next one able to do the
//! same work, and one dead feed must never stop the other forty — which is why
//! [`crate::service::poll_due_sources`] records a failure and backs the source off
//! rather than returning an error for the whole pass.

use std::time::Duration;

use crate::{
    models::{now_ms, BriefTrigger},
    service,
    state::AppState,
};

/// How often the loop wakes.
///
/// This is NOT the per-source poll interval — that lives in each workspace's settings
/// and is enforced by `due_sources`, which only returns a source whose `next_fetch_at`
/// has passed. This is just how often the process checks whether anything is due, so
/// it can be short without hammering anybody's server.
pub const LOOP_PERIOD_SECS: u64 = 60;

/// Run the poll loop until the task is aborted.
pub async fn run(state: AppState) {
    let period = Duration::from_secs(LOOP_PERIOD_SECS);
    let mut ticker = tokio::time::interval(period);
    loop {
        // The first tick fires immediately, so a lazily spawned sidecar catches up on
        // whatever came out while it was stopped — which, being `lazy`, is the normal
        // case rather than an edge one.
        ticker.tick().await;
        if let Err(err) = once(&state).await {
            // Never propagate: one transient failure must not kill the loop and stop
            // every future poll with nothing in the log after the first line.
            tracing::warn!(error = %err, "news: poll pass failed");
        }
    }
}

/// One pass. Separated from the loop so it is callable directly in a test.
pub async fn once(state: &AppState) -> anyhow::Result<()> {
    let now = now_ms();
    let report = service::poll_due_sources(state, now).await?;
    if report.articles_new > 0 || report.failures > 0 {
        tracing::info!(
            polled = report.sources_polled,
            new = report.articles_new,
            duplicates = report.duplicates,
            opened = report.stories_opened,
            joined = report.stories_joined,
            matches = report.topic_matches,
            bursts = report.bursts,
            failures = report.failures,
            "news: poll pass"
        );
    }
    maybe_write_brief(state, now).await;
    Ok(())
}

/// Write the scheduled brief when the local wall-clock time has come round.
///
/// The check is "has the brief hour passed today, and have we not written one today",
/// not "is it exactly 07:30" — a poll interval of five minutes would otherwise miss
/// the window entirely on a machine that was asleep at 07:30, which is most of them.
async fn maybe_write_brief(state: &AppState, now: i64) {
    let Ok(workspaces) = state.store.list_workspaces().await else {
        return;
    };
    for workspace in workspaces {
        let Ok(settings) = state.store.get_settings(&workspace.id).await else {
            continue;
        };
        let Some(at) = settings.brief_time.as_deref() else {
            continue;
        };
        let tz = resolve_tz(settings.brief_timezone.as_deref().unwrap_or(""));
        let Some(due_at) = local_time_today_ms(tz, at, now) else {
            // An unparseable time is a settings problem, not a reason to retry every
            // minute forever. Logged once per pass and skipped.
            tracing::warn!(brief_time = at, "news: the brief time could not be read");
            continue;
        };
        if now < due_at {
            continue;
        }
        // One brief per local day: the latest one already covering today's window is
        // what stops a five-minute poll writing a brief every five minutes after 07:30.
        if let Ok(Some(latest)) = state.store.latest_brief(&workspace.id).await {
            if latest.generated_at >= due_at {
                continue;
            }
        }
        match service::generate_brief(state, BriefTrigger::Scheduled, now).await {
            Ok(brief) => tracing::info!(brief = %brief.id, "news: wrote the scheduled brief"),
            Err(err) => tracing::warn!(error = %err, "news: could not write the scheduled brief"),
        }
    }
}

/// Resolve an IANA zone name, falling back to UTC.
///
/// Never fails: a zone string that was hand-edited to nonsense must not stop the brief
/// from ever being written.
fn resolve_tz(name: &str) -> chrono_tz::Tz {
    name.parse().unwrap_or(chrono_tz::UTC)
}

/// The epoch-millis instant of `HH:MM` local time on the day `now` falls in.
///
/// Returns `None` for an unparseable time. A DST gap (the local time does not exist
/// that day) resolves to the first valid instant after it, which is the behaviour a
/// person expects from "write the brief at 02:30" on a spring-forward morning: they
/// get it as soon as 02:30 would have happened, not never.
fn local_time_today_ms(tz: chrono_tz::Tz, hhmm: &str, now: i64) -> Option<i64> {
    use chrono::TimeZone as _;
    let (hours, minutes) = hhmm.trim().split_once(':')?;
    let hours: u32 = hours.trim().parse().ok()?;
    let minutes: u32 = minutes.trim().parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let local_now = chrono::DateTime::from_timestamp_millis(now)?.with_timezone(&tz);
    let date = local_now.date_naive();
    let naive = date.and_hms_opt(hours, minutes, 0)?;
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(at) => Some(at.timestamp_millis()),
        chrono::LocalResult::Ambiguous(earlier, _) => Some(earlier.timestamp_millis()),
        // The gap case: step forward an hour and take that.
        chrono::LocalResult::None => {
            let shifted = naive.checked_add_signed(chrono::Duration::hours(1))?;
            tz.from_local_datetime(&shifted)
                .single()
                .map(|at| at.timestamp_millis())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_brief_time_resolves_against_the_configured_zone() {
        // 2026-08-10T00:00:00Z. In Singapore that is already 08:00 the same day, so
        // "07:30 Singapore" is BEHIND `now` — which is exactly the case that must
        // trigger the brief rather than wait until tomorrow.
        let now = 1_786_320_000_000;
        let sg = resolve_tz("Asia/Singapore");
        let at = local_time_today_ms(sg, "07:30", now).expect("a valid time");
        assert!(at < now, "07:30 SGT precedes 08:00 SGT");

        // The same wall-clock time in UTC is ahead of it.
        let utc = local_time_today_ms(resolve_tz("UTC"), "07:30", now).expect("valid");
        assert!(utc > now);
    }

    #[test]
    fn an_unreadable_brief_time_is_none_rather_than_a_default() {
        let tz = resolve_tz("UTC");
        let now = 1_786_320_000_000;
        for bad in ["", "half seven", "25:00", "07:99", "0730"] {
            assert!(
                local_time_today_ms(tz, bad, now).is_none(),
                "'{bad}' must not resolve"
            );
        }
    }

    #[test]
    fn an_unknown_zone_falls_back_to_utc_rather_than_failing() {
        // A hand-edited zone string must not stop the brief being written forever.
        assert_eq!(resolve_tz("Mars/Olympus"), chrono_tz::UTC);
        assert_eq!(resolve_tz(""), chrono_tz::UTC);
    }

    #[tokio::test]
    async fn a_pass_against_an_empty_store_does_nothing_and_does_not_fail() {
        // The first pass of a freshly installed app: no sources, no workspace content.
        // It must be a no-op rather than an error, because an error here would be
        // logged every interval forever on a node nobody has configured yet.
        let store = crate::store::NewsStore::open_in_memory().expect("a store");
        let state = AppState::new(store, crate::state::Config::from_env(8008));
        once(&state).await.expect("an empty pass must succeed");
    }
}
