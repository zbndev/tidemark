//! How a reading is presented, shared by every process that puts one in front of a person.
//!
//! Presentation, and deliberately in the shared crate rather than in the interface: the
//! card, the notification and the tray menu all say how full a window is and how long it
//! has left, and they do not all run in the same process. Two spellings of the same number
//! read as two different numbers — `1 h 12 min` on the card next to `72 minutes` in the
//! notification is a bug the user has no way to diagnose.
//!
//! The same argument covers the provider's mark: the card and the notification look it up
//! under one name or they show two different pictures of one service.
//!
//! Nothing here reaches for the clock. Every function is a pure function of numbers the
//! daemon already sent, which is what makes the awkward cases — an overdue reset, a window
//! that is not quite empty — testable without waiting for them.

/// Consumption as the big number on the card, and as the number a notification leads with.
///
/// Rounds, but never across the ends: a window with something spent in it never reads `0%`,
/// and one with anything left never reads `100%`. Those two are the readings a person acts
/// on, and rounding is not a good enough reason to get either wrong.
pub fn percent(used_percent: f64) -> String {
    let used = used_percent.clamp(0.0, 100.0);
    let rounded = used.round();
    if rounded <= 0.0 && used > 0.0 {
        "<1%".to_owned()
    } else if rounded >= 100.0 && used < 100.0 {
        ">99%".to_owned()
    } else {
        format!("{rounded:.0}%")
    }
}

/// A span of time, at the coarsest unit that still says something useful.
pub fn duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;

    if minutes == 0 {
        "under a minute".to_owned()
    } else if hours == 0 {
        format!("{minutes} min")
    } else if days == 0 {
        match minutes % 60 {
            0 => format!("{hours} h"),
            rest => format!("{hours} h {rest} min"),
        }
    } else {
        match hours % 24 {
            0 => plural(days, "day"),
            rest => format!("{} {rest} h", plural(days, "day")),
        }
    }
}

/// A count with its unit, pluralised.
pub fn plural(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

/// The icon name a provider's mark is installed under, or `None` for a slug that cannot
/// name one.
///
/// The slug arrives over D-Bus from the daemon, so it is not assumed to be one of ours.
/// Anything outside `[a-z0-9-]` gets no mark rather than a lookup for a name we would never
/// have installed. Marks live in `hicolor` under `symbolic/apps/tidemark-<slug>-symbolic`;
/// see `mark.rs` in the interface crate for why they are asked for by name.
pub fn icon_name(slug: &str) -> Option<String> {
    let usable = !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    usable.then(|| format!("tidemark-{slug}-symbolic"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_never_reports_an_untouched_window_or_an_exhausted_one_by_mistake() {
        assert_eq!(percent(0.0), "0%");
        assert_eq!(
            percent(0.2),
            "<1%",
            "something was spent; do not print zero"
        );
        assert_eq!(
            percent(99.7),
            ">99%",
            "there is quota left; do not print 100"
        );
        assert_eq!(percent(100.0), "100%");
        assert_eq!(percent(42.4), "42%");
        assert_eq!(percent(42.5), "43%");
    }

    #[test]
    fn a_percentage_outside_the_range_is_clamped_rather_than_printed() {
        assert_eq!(percent(140.0), "100%");
        assert_eq!(percent(-3.0), "0%");
    }

    #[test]
    fn a_mark_is_asked_for_by_the_name_it_is_installed_under() {
        assert_eq!(
            icon_name("zai").as_deref(),
            Some("tidemark-zai-symbolic"),
            "the name here and the filename in data/icons are the same convention"
        );
        assert_eq!(
            icon_name("antigravity").as_deref(),
            Some("tidemark-antigravity-symbolic")
        );
    }

    #[test]
    fn a_slug_that_could_not_name_an_installed_file_names_no_mark() {
        for slug in ["", "Z.ai", "../../etc", "zai fake", "ZAI"] {
            assert_eq!(icon_name(slug), None, "slug {slug:?} should name no icon");
        }
    }

    #[test]
    fn durations_stop_at_the_unit_that_still_means_something() {
        assert_eq!(duration(30), "under a minute");
        assert_eq!(duration(90), "1 min");
        assert_eq!(duration(3600), "1 h");
        assert_eq!(duration(3600 + 12 * 60), "1 h 12 min");
        assert_eq!(duration(48 * 3600), "2 days");
        assert_eq!(duration(50 * 3600), "2 days 2 h");
        assert_eq!(duration(-5), "under a minute");
    }
}
