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

use crate::{Field, Format, Metric};

/// One reported field, spelled in the producer's semantic format and our house style.
///
/// The format says how to spell a number; the *field* says what kind of quantity it is, and
/// only the field can say that. A metric denominated in `USD` still measures its fullness in
/// percent, so the unit belongs to the amounts — value, maximum, remaining — and never to
/// `used_percent`, which would otherwise read `25 USD`. For the same reason the 0-100 clamp
/// in [`percent`] belongs to `used_percent` alone: it exists so a window that is not quite
/// empty never reads `100%`, and applying it to an amount the producer reported would print
/// a number nobody sent.
pub fn format_field(metric: &Metric, field: Field, format: Option<Format>) -> Option<String> {
    if field == Field::Text {
        return metric.text.clone();
    }
    let number = metric.field(field)?;
    let fullness = field == Field::UsedPercent;
    Some(match format.unwrap_or(Format::Number) {
        Format::Percent if fullness => percent(number),
        Format::Percent => format!("{}%", trim_number(number)),
        Format::Currency => match metric.unit.as_deref() {
            Some(unit) => format!("{number:.2} {unit}"),
            None => format!("{number:.2}"),
        },
        Format::Duration => duration(number.round() as i64),
        Format::Text => trim_number(number),
        Format::Number if fullness => format!("{}%", trim_number(number)),
        Format::Number => match metric.unit.as_deref() {
            Some(unit) => format!("{} {unit}", trim_number(number)),
            None => trim_number(number),
        },
    })
}

/// Two reported fields as `X of Y`, in the metric's unit.
pub fn format_ratio(metric: &Metric, left: Field, right: Field) -> Option<String> {
    let (left, right) = (metric.field(left)?, metric.field(right)?);
    let body = format!("{} of {}", trim_number(left), trim_number(right));
    Some(match metric.unit.as_deref() {
        Some(unit) => format!("{body} {unit}"),
        None => body,
    })
}

fn trim_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{number:.0}")
    } else {
        let rendered = format!("{number:.2}");
        rendered
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

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

/// The icon-theme slug an installed plugin's mark is filed under.
pub fn plugin_icon_slug(provider_id: &str) -> Option<String> {
    let usable = !provider_id.is_empty()
        && provider_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-');
    usable.then(|| provider_id.replace('.', "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric() -> crate::Metric {
        crate::Metric {
            id: "cost".into(),
            title: "Monthly cost".into(),
            subtitle: None,
            value: Some(12.5),
            maximum: Some(50.0),
            remaining: Some(37.5),
            used_percent: Some(25.0),
            text: Some("active".into()),
            unit: Some("USD".into()),
            window: None,
        }
    }

    #[test]
    fn a_field_is_spelled_by_the_format_the_producer_chose() {
        use crate::{Field, Format};
        let m = metric();
        assert_eq!(
            format_field(&m, Field::UsedPercent, Some(Format::Percent)).as_deref(),
            Some("25%")
        );
        assert_eq!(
            format_field(&m, Field::Value, Some(Format::Currency)).as_deref(),
            Some("12.50 USD")
        );
        assert_eq!(
            format_field(&m, Field::Text, Some(Format::Text)).as_deref(),
            Some("active")
        );
    }

    #[test]
    fn a_field_is_spelled_as_the_kind_of_quantity_it_is() {
        use crate::{Field, Format};
        let m = metric();
        assert_eq!(
            format_field(&m, Field::UsedPercent, Some(Format::Number)).as_deref(),
            Some("25%"),
            "a percentage is not denominated in the metric's unit"
        );
        assert_eq!(
            format_field(&m, Field::Remaining, Some(Format::Number)).as_deref(),
            Some("37.5 USD"),
            "an amount still carries the unit"
        );
    }

    #[test]
    fn only_a_used_percentage_is_clamped_to_the_ends_of_a_window() {
        use crate::{Field, Format};
        let over = crate::Metric {
            value: Some(150.0),
            ..metric()
        };
        assert_eq!(
            format_field(&over, Field::Value, Some(Format::Percent)).as_deref(),
            Some("150%"),
            "an amount the producer reported is not rewritten to fit 0-100"
        );
        let overfull = crate::Metric {
            used_percent: Some(150.0),
            ..metric()
        };
        assert_eq!(
            format_field(&overfull, Field::UsedPercent, Some(Format::Percent)).as_deref(),
            Some("100%"),
            "a window's own fullness still stops at full"
        );
    }

    #[test]
    fn a_field_the_producer_never_reported_is_spelled_as_nothing() {
        use crate::{Field, Format};
        let bare = crate::Metric {
            remaining: None,
            ..metric()
        };
        assert_eq!(
            format_field(&bare, Field::Remaining, Some(Format::Number)),
            None
        );
    }

    #[test]
    fn a_ratio_needs_both_operands() {
        use crate::Field;
        let m = metric();
        assert_eq!(
            format_ratio(&m, Field::Value, Field::Maximum).as_deref(),
            Some("12.5 of 50 USD")
        );
        let bare = crate::Metric {
            maximum: None,
            ..metric()
        };
        assert_eq!(format_ratio(&bare, Field::Value, Field::Maximum), None);
    }

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
    fn a_plugin_id_names_a_mark_through_the_same_lookup_a_built_in_does() {
        let slug = plugin_icon_slug("com.acme.quota").expect("usable");
        assert_eq!(slug, "com-acme-quota");
        assert_eq!(
            icon_name(&slug).as_deref(),
            Some("tidemark-com-acme-quota-symbolic")
        );
        assert_eq!(plugin_icon_slug("../../etc/passwd"), None);
        assert_eq!(plugin_icon_slug("Com.Acme"), None);
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
