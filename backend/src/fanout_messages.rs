//! Inbox, toast and push copy as localizable messages.
//!
//! Core renders a notify hint's `i18n` text per recipient (the inbox row and
//! toast in the node's `ui-language`, each push in the locale its device
//! registered with); the English rendering still goes out as the plain
//! `title`/`body`, which an older Core delivers as is.
//!
//! Each `*_MESSAGE` is `format!` with one named argument per placeholder
//! (`name = arg("name")`) so the i18n extractor catalogs the ICU source;
//! `tools/generate-core-i18n.mjs` copies the translations into Core's catalog.
//! Every message is a whole sentence. Core's renderer has no plural selector,
//! so counts are written as `label: {count}`.
#![allow(clippy::useless_format)]

use ryu_app_events::{NotifyHint, NotifyMessage, NotifyTemplate as Template};

pub(crate) const TOPIC_BREAKING_MESSAGE: Template = |_| format!("A watched topic is breaking");
pub(crate) const STORIES_LAST_HOUR_MESSAGE: Template =
    |arg| format!("Stories in the last hour: {count}", count = arg("count"));
pub(crate) const BRIEF_READY_MESSAGE: Template = |_| format!("Your news brief is ready");
pub(crate) const BRIEF_STORIES_MESSAGE: Template =
    |arg| format!("Stories in this brief: {count}", count = arg("count"));

/// A watched topic is breaking. Names no topic (the watch name is sealed).
pub(crate) fn topic_breaking(count: i64) -> NotifyHint {
    NotifyHint::localized(
        NotifyMessage::new(TOPIC_BREAKING_MESSAGE),
        vec![NotifyMessage::new(STORIES_LAST_HOUR_MESSAGE).count("count", count)],
    )
    .with_level("warning")
}

/// A scheduled brief is ready.
pub(crate) fn brief_ready(story_count: usize) -> NotifyHint {
    NotifyHint::localized(
        NotifyMessage::new(BRIEF_READY_MESSAGE),
        vec![NotifyMessage::new(BRIEF_STORIES_MESSAGE)
            .count("count", i64::try_from(story_count).unwrap_or(i64::MAX))],
    )
    .with_level("success")
}
