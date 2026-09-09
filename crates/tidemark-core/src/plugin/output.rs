//! The Lua return value, turned into types the daemon can publish — or refused whole.
//!
//! Refused *whole* is the rule that matters. A reading with one bad metric is not published
//! with the rest of it: the account keeps its last good reading behind a failure state, which
//! is the same thing that happens when a built-in provider's response stops making sense.
//! Half a reading is the one outcome that would put a confident wrong number on the card.
//!
//! Everything a widget needs is checked here rather than in the renderer, so the GUI never
//! has to decide what to do about a gauge with no denominator.

use super::lua::Executed;
use super::lua::api::MARKER;
use super::{PluginError, Reading, limits};
use mlua::{Table, Value};
use tidemark_types::{
    AccountId, Emphasis, Field, Format, Metric, MetricWindow, Presentation, PresentedSection,
    ProviderId, Snapshot, Timestamp, Widget, WidgetKind, Window, WindowKey, WindowLength,
};

/// How deep a returned table may nest before it is called an attack rather than a mistake.
const MAX_DEPTH: usize = 16;

/// Validates one execution's result.
pub fn validate(
    executed: &Executed,
    provider: &ProviderId,
    account: &AccountId,
    captured_at: Timestamp,
) -> Result<Reading, PluginError> {
    let Value::Table(root) = &executed.value else {
        return Err(refuse(
            "parse must return a table with metrics, card and details",
        ));
    };

    let metrics = sequence(root, "metrics")?;
    if metrics.len() > limits::MAX_METRICS {
        return Err(refuse(format!(
            "a reading may carry {} metrics, and this one carries {}",
            limits::MAX_METRICS,
            metrics.len()
        )));
    }

    let mut parsed: Vec<Metric> = Vec::with_capacity(metrics.len());
    let mut windows: Vec<Window> = Vec::new();
    for entry in metrics {
        let Value::Table(entry) = entry else {
            return Err(refuse("every metrics entry must be a table"));
        };
        depth_ok(&entry, 0)?;
        let metric = metric(&entry)?;
        if parsed.iter().any(|existing| existing.id == metric.id) {
            return Err(refuse(format!("metric id {} appears twice", metric.id)));
        }
        if let Some(window) = window(&metric)? {
            windows.push(window);
        }
        parsed.push(metric);
    }

    let card = widgets(root, "card", limits::MAX_CARD_ITEMS, &parsed)?;

    let sections = sequence(root, "details")?;
    if sections.len() > limits::MAX_DETAIL_SECTIONS {
        return Err(refuse(format!(
            "a reading may carry {} detail sections",
            limits::MAX_DETAIL_SECTIONS
        )));
    }
    let mut details = Vec::with_capacity(sections.len());
    for section in sections {
        let Value::Table(section) = section else {
            return Err(refuse("every details entry must be a table"));
        };
        let title = text(&section, "title", limits::MAX_LABEL_BYTES)?
            .ok_or_else(|| refuse("a detail section must have a title"))?;
        let items = widgets(&section, "items", limits::MAX_SECTION_ITEMS, &parsed)?;
        details.push(PresentedSection { title, items });
    }

    Ok(Reading {
        snapshot: Snapshot {
            provider: provider.clone(),
            account: account.clone(),
            captured_at,
            windows,
            // Plugins express detail through the presentation. The legacy `DetailSection`
            // list stays empty rather than being duplicated into: two orders over one
            // reading is two orders to keep in step.
            details: Vec::new(),
        },
        presentation: Presentation {
            metrics: parsed,
            card,
            details,
        },
    })
}

/// One metric.
fn metric(entry: &Table) -> Result<Metric, PluginError> {
    let id = text(entry, "id", limits::MAX_ID_BYTES)?
        .filter(|id| !id.is_empty())
        .ok_or_else(|| refuse("every metric needs a non-empty id"))?;
    let title = text(entry, "title", limits::MAX_LABEL_BYTES)?
        .ok_or_else(|| refuse(format!("metric {id} needs a title")))?;
    Ok(Metric {
        id,
        title,
        subtitle: text(entry, "subtitle", limits::MAX_LABEL_BYTES)?,
        value: number(entry, "value")?,
        maximum: number(entry, "maximum")?,
        remaining: number(entry, "remaining")?,
        used_percent: number(entry, "used_percent")?,
        text: text(entry, "text", limits::MAX_TEXT_BYTES)?,
        unit: text(entry, "unit", limits::MAX_LABEL_BYTES)?,
        window: window_metadata(entry)?,
    })
}

/// A metric's window metadata, when it declared any.
fn window_metadata(entry: &Table) -> Result<Option<MetricWindow>, PluginError> {
    let value: Value = entry.get("window").map_err(lua_shape)?;
    let table = match value {
        Value::Nil => return Ok(None),
        Value::Table(table) => table,
        _ => return Err(refuse("a metric's window must be a table")),
    };
    let key = text(&table, "key", limits::MAX_ID_BYTES)?
        .filter(|key| !key.is_empty())
        .ok_or_else(|| refuse("a window needs a stable key"))?;
    let resets_at = integer(&table, "resets_at")?;
    if let Some(seconds) = resets_at {
        Timestamp::from_unix(seconds)
            .map_err(|_| refuse("a window's resets_at is not a plausible time"))?;
    }
    let length_secs = match integer(&table, "length_seconds")? {
        None => None,
        Some(seconds) if seconds > 0 => Some(seconds as u64),
        Some(_) => return Err(refuse("a window's length_seconds must be positive")),
    };
    Ok(Some(MetricWindow {
        key,
        resets_at,
        length_secs,
    }))
}

/// The domain window a windowed metric produces.
fn window(metric: &Metric) -> Result<Option<Window>, PluginError> {
    let Some(declared) = metric.window.as_ref() else {
        return Ok(None);
    };
    let used_percent = metric
        .used_percent
        .filter(|used| used.is_finite())
        .ok_or_else(|| {
            refuse(format!(
                "metric {} declares a window and no used_percent; a window with no consumption \
                 would be drawn as an empty one",
                metric.id
            ))
        })?;
    Ok(Some(Window {
        key: WindowKey::named(&declared.key),
        title: metric.title.clone(),
        subtitle: metric.subtitle.clone(),
        used_percent,
        resets_at: declared
            .resets_at
            .and_then(|s| Timestamp::from_unix(s).ok()),
        length: declared.length_secs.and_then(WindowLength::from_secs),
    }))
}

/// One ordered widget list, checked against the metrics it references.
fn widgets(
    root: &Table,
    field: &str,
    limit: usize,
    metrics: &[Metric],
) -> Result<Vec<Widget>, PluginError> {
    let entries = sequence(root, field)?;
    if entries.len() > limit {
        return Err(refuse(format!("{field} may carry {limit} items")));
    }
    let mut widgets = Vec::with_capacity(entries.len());
    for entry in entries {
        let Value::Table(entry) = entry else {
            return Err(refuse(format!(
                "every {field} item must come from gauge, value, ratio or status"
            )));
        };
        let kind = text(&entry, MARKER, limits::MAX_ID_BYTES)?
            .and_then(|kind| WidgetKind::from_wire(&kind))
            .ok_or_else(|| {
                refuse(format!(
                    "every {field} item must come from gauge, value, ratio or status"
                ))
            })?;
        let metric_id = text(&entry, "metric", limits::MAX_ID_BYTES)?
            .ok_or_else(|| refuse("a widget must name a metric"))?;
        let metric = metrics
            .iter()
            .find(|metric| metric.id == metric_id)
            .ok_or_else(|| refuse(format!("no metric is called {metric_id}")))?;

        let widget = Widget {
            kind: kind.as_wire().to_owned(),
            metric: metric_id,
            field: field_name(&entry, "field")?,
            left: field_name(&entry, "left")?,
            right: field_name(&entry, "right")?,
            format: choice(&entry, "format", |name| {
                Format::from_wire(name).map(Format::as_wire)
            })?,
            emphasis: choice(&entry, "emphasis", |name| {
                Emphasis::from_wire(name).map(Emphasis::as_wire)
            })?,
        };
        check_drawable(kind, metric, &widget)?;
        widgets.push(widget);
    }
    Ok(widgets)
}

/// Whether a widget has the numbers it claims to draw.
fn check_drawable(kind: WidgetKind, metric: &Metric, widget: &Widget) -> Result<(), PluginError> {
    let id = &metric.id;
    match kind {
        WidgetKind::Gauge => {
            if metric.used_percent.is_some_and(f64::is_finite) {
                return Ok(());
            }
            let left = widget.left().unwrap_or(Field::Value);
            let right = widget.right().unwrap_or(Field::Maximum);
            match (metric.field(left), metric.field(right)) {
                (Some(_), Some(denominator)) if denominator > 0.0 => Ok(()),
                (Some(_), Some(_)) => Err(refuse(format!(
                    "the gauge over {id} divides by a {right} that is not positive"
                ))),
                _ => Err(refuse(format!(
                    "the gauge over {id} needs a finite used_percent, or a numerator and a \
                     positive denominator"
                ))),
            }
        }
        WidgetKind::Ratio => {
            let (left, right) = (
                widget
                    .left()
                    .ok_or_else(|| refuse(format!("the ratio over {id} needs a left field")))?,
                widget
                    .right()
                    .ok_or_else(|| refuse(format!("the ratio over {id} needs a right field")))?,
            );
            match (metric.field(left), metric.field(right)) {
                (Some(_), Some(_)) => Ok(()),
                _ => Err(refuse(format!("the ratio over {id} is missing an operand"))),
            }
        }
        WidgetKind::Value => {
            match widget.field().unwrap_or(Field::Value) {
                Field::Text => metric.text.as_ref().map(|_| ()).ok_or_else(|| {
                    refuse(format!("the value over {id} reads text it does not have"))
                }),
                selected => metric.field(selected).map(|_| ()).ok_or_else(|| {
                    refuse(format!(
                        "the value over {id} reads {selected}, which is absent"
                    ))
                }),
            }
        }
        WidgetKind::Status => metric
            .text
            .as_ref()
            .map(|_| ())
            .ok_or_else(|| refuse(format!("the status over {id} has no text"))),
    }
}

/// A one-based sequence field, refusing anything else.
fn sequence(root: &Table, field: &str) -> Result<Vec<Value>, PluginError> {
    let value: Value = root.get(field).map_err(lua_shape)?;
    let Value::Table(table) = value else {
        return Err(refuse(format!("{field} must be an array")));
    };
    table
        .sequence_values::<Value>()
        .collect::<mlua::Result<Vec<_>>>()
        .map_err(lua_shape)
}

/// A bounded string field.
fn text(table: &Table, field: &str, limit: usize) -> Result<Option<String>, PluginError> {
    match table.get::<Value>(field).map_err(lua_shape)? {
        Value::Nil => Ok(None),
        Value::String(text) => {
            let text = text.to_str().map_err(lua_shape)?.to_string();
            if text.len() > limit {
                return Err(refuse(format!("{field} is longer than {limit} bytes")));
            }
            Ok(Some(text))
        }
        other => Err(refuse(format!(
            "{field} must be a string, not a {}",
            other.type_name()
        ))),
    }
}

/// A finite numeric field.
fn number(table: &Table, field: &str) -> Result<Option<f64>, PluginError> {
    match table.get::<Value>(field).map_err(lua_shape)? {
        Value::Nil => Ok(None),
        Value::Integer(integer) => Ok(Some(integer as f64)),
        Value::Number(number) if number.is_finite() => Ok(Some(number)),
        Value::Number(_) => Err(refuse(format!("{field} is not a finite number"))),
        other => Err(refuse(format!(
            "{field} must be a number, not a {}",
            other.type_name()
        ))),
    }
}

/// An integer field.
fn integer(table: &Table, field: &str) -> Result<Option<i64>, PluginError> {
    match number(table, field)? {
        None => Ok(None),
        Some(number) if number.fract() == 0.0 => Ok(Some(number as i64)),
        Some(_) => Err(refuse(format!("{field} must be a whole number of seconds"))),
    }
}

/// A field name from the documented set.
fn field_name(table: &Table, key: &str) -> Result<Option<String>, PluginError> {
    choice(table, key, |name| {
        Field::from_wire(name).map(Field::as_wire)
    })
}

/// A named choice from one of the presentation enums, canonicalised to its wire spelling.
///
/// Written once over a lookup function rather than over a trait: the three enums already
/// have the same pair of inherent methods, and a trait wrapping them would only forward.
fn choice(
    table: &Table,
    key: &str,
    known: fn(&str) -> Option<&'static str>,
) -> Result<Option<String>, PluginError> {
    match text(table, key, limits::MAX_ID_BYTES)? {
        None => Ok(None),
        Some(name) => known(&name)
            .map(|wire| Some(wire.to_owned()))
            .ok_or_else(|| refuse(format!("{name} is not a {key} this build knows"))),
    }
}

/// Refuses a table that nests deeply enough to be a cycle. A cyclic table has no depth, so
/// this is also the cycle check: a loop hits the bound before anything recurses far enough
/// to matter.
fn depth_ok(table: &Table, depth: usize) -> Result<(), PluginError> {
    if depth > MAX_DEPTH {
        return Err(refuse(
            "a returned table nests too deeply, or refers to itself",
        ));
    }
    for entry in table.clone().pairs::<Value, Value>() {
        let (_, value) = entry.map_err(lua_shape)?;
        match value {
            Value::Table(nested) => depth_ok(&nested, depth + 1)?,
            Value::Function(_)
            | Value::Thread(_)
            | Value::UserData(_)
            | Value::LightUserData(_) => {
                return Err(refuse(
                    "a reading may not contain a function, a thread or userdata",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// One refusal, bounded so a hostile document cannot write the diagnostic.
fn refuse(reason: impl Into<String>) -> PluginError {
    let mut reason: String = reason.into();
    reason.truncate(limits::MAX_ERROR_BYTES);
    PluginError::Output { reason }
}

/// A Lua error raised while reading the result is a shape problem, not a runtime one: the
/// chunk has already returned.
fn lua_shape(error: mlua::Error) -> PluginError {
    refuse(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::lua;
    use serde_json::json;

    const AT: i64 = 1_788_870_896;

    fn reading(body: &str) -> Result<Reading, PluginError> {
        let source = format!("function parse(response, context)\n{body}\nend");
        let executed = lua::run(&source, &json!({}), AT)?;
        validate(
            &executed,
            &ProviderId::new("com.acme.quota"),
            &AccountId::default(),
            Timestamp::from_unix(AT).expect("plausible"),
        )
    }

    const ONE_METRIC: &str = r#"
    return {
        metrics = {
            {
                id = "cost",
                title = "Monthly cost",
                value = 12.5,
                maximum = 50,
                remaining = 37.5,
                used_percent = 25,
                unit = "USD",
            },
        },
        card = { gauge("cost", { field = "used_percent" }) },
        details = { { title = "Limits", items = { ratio("cost", { left = "value", right = "maximum" }) } } },
    }
    "#;

    #[test]
    fn a_complete_result_becomes_a_presentation_and_a_snapshot() {
        let reading = reading(ONE_METRIC).expect("the documented shape validates");
        let metric = reading.presentation.metric("cost").expect("present");
        assert_eq!(metric.title, "Monthly cost");
        assert_eq!(metric.value, Some(12.5));
        assert_eq!(metric.unit.as_deref(), Some("USD"));
        assert_eq!(reading.presentation.card.len(), 1);
        assert_eq!(reading.presentation.details[0].title, "Limits");
        assert!(
            reading.snapshot.windows.is_empty(),
            "a metric with no window metadata is presentational and never becomes history"
        );
    }

    #[test]
    fn the_same_metric_can_be_drawn_three_ways_without_being_parsed_again() {
        let reading = reading(
            r#"
            return {
                metrics = { { id = "cost", title = "Cost", value = 1, maximum = 4, remaining = 3, used_percent = 25 } },
                card = {
                    gauge("cost", { field = "used_percent" }),
                    value("cost", { field = "remaining" }),
                    ratio("cost", { left = "value", right = "maximum" }),
                },
                details = {},
            }
            "#,
        )
        .expect("validates");
        assert_eq!(
            reading.presentation.metrics.len(),
            1,
            "one metric, three widgets"
        );
        let kinds: Vec<&str> = reading
            .presentation
            .card
            .iter()
            .map(|w| w.kind.as_str())
            .collect();
        assert_eq!(
            kinds,
            ["gauge", "value", "ratio"],
            "and the order is the plugin's"
        );
    }

    #[test]
    fn a_windowed_metric_becomes_a_window_with_exactly_the_metadata_it_declared() {
        let reading = reading(
            r#"
            return {
                metrics = {
                    {
                        id = "five-hours",
                        title = "5 hours",
                        used_percent = 42,
                        window = { key = "w18000", resets_at = 1788874496, length_seconds = 18000 },
                    },
                    {
                        id = "monthly",
                        title = "Monthly",
                        used_percent = 5,
                        window = { key = "monthly" },
                    },
                },
                card = { gauge("five-hours", { field = "used_percent" }) },
                details = {},
            }
            "#,
        )
        .expect("validates");
        let windows = &reading.snapshot.windows;
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].key.as_str(), "w18000");
        assert_eq!(windows[0].used_percent, 42.0);
        assert_eq!(windows[0].length.map(|l| l.as_secs()), Some(18_000));
        assert_eq!(
            windows[0].resets_at.map(|t| t.as_unix()),
            Some(1_788_874_496)
        );
        assert_eq!(
            windows[1].resets_at, None,
            "no reset was declared, so none is published"
        );
        assert_eq!(windows[1].length, None);
    }

    #[test]
    fn a_windowed_metric_without_a_percentage_is_refused_rather_than_drawn_at_zero() {
        let error = reading(
            r#"
            return {
                metrics = { { id = "m", title = "M", window = { key = "w60" } } },
                card = {},
                details = {},
            }
            "#,
        )
        .expect_err("a window with no consumption is not a window");
        assert!(matches!(error, PluginError::Output { .. }));
    }

    #[test]
    fn every_refusable_shape_is_refused() {
        for (name, body) in [
            (
                "a dangling card reference",
                r#"return { metrics = {}, card = { gauge("ghost", {}) }, details = {} }"#,
            ),
            (
                "a duplicate metric id",
                r#"return { metrics = { { id = "a", title = "A", used_percent = 1 }, { id = "a", title = "B", used_percent = 2 } }, card = {}, details = {} }"#,
            ),
            (
                "a malformed metric id",
                r#"return { metrics = { { id = "", title = "A" } }, card = {}, details = {} }"#,
            ),
            (
                "a gauge with no percentage and no denominator",
                r#"return { metrics = { { id = "a", title = "A", value = 1 } }, card = { gauge("a", { field = "used_percent" }) }, details = {} }"#,
            ),
            (
                "a gauge whose denominator is zero",
                r#"return { metrics = { { id = "a", title = "A", value = 1, maximum = 0 } }, card = { gauge("a", { left = "value", right = "maximum" }) }, details = {} }"#,
            ),
            (
                "a ratio missing an operand",
                r#"return { metrics = { { id = "a", title = "A", value = 1 } }, card = { ratio("a", { left = "value", right = "maximum" }) }, details = {} }"#,
            ),
            (
                "a field name this build does not have",
                r#"return { metrics = { { id = "a", title = "A", value = 1 } }, card = { value("a", { field = "vlaue" }) }, details = {} }"#,
            ),
            (
                "a widget that is not one of the four",
                r#"return { metrics = { { id = "a", title = "A" } }, card = { { __widget = "hologram", metric = "a" } }, details = {} }"#,
            ),
            (
                "a card item that is not a widget at all",
                r#"return { metrics = {}, card = { "a string" }, details = {} }"#,
            ),
            (
                "a malformed window length",
                r#"return { metrics = { { id = "a", title = "A", used_percent = 1, window = { key = "w", length_seconds = 0 } } }, card = {}, details = {} }"#,
            ),
            (
                "a section with no title",
                r#"return { metrics = {}, card = {}, details = { { items = {} } } }"#,
            ),
            ("a result that is not a table", "return 4"),
            (
                "a result with no metrics field",
                "return { card = {}, details = {} }",
            ),
            (
                "a function in the result",
                r#"return { metrics = { { id = "a", title = tostring } }, card = {}, details = {} }"#,
            ),
        ] {
            let error = reading(body);
            assert!(
                matches!(error, Err(PluginError::Output { .. })),
                "{name} must be refused, got {error:?}"
            );
        }
    }

    #[test]
    fn a_cyclic_table_is_refused_rather_than_recursed_into() {
        let error = reading(
            r#"
            local loop = { id = "a", title = "A", used_percent = 1 }
            loop.window = { key = "w", nested = loop }
            return { metrics = { loop }, card = {}, details = {} }
            "#,
        );
        assert!(
            matches!(error, Err(PluginError::Output { .. })),
            "{error:?}"
        );
    }

    #[test]
    fn a_percentage_above_one_hundred_is_published_as_the_number_it_is() {
        let reading = reading(
            r#"return { metrics = { { id = "a", title = "A", used_percent = 140, window = { key = "w" } } }, card = {}, details = {} }"#,
        )
        .expect("140% is truthful data");
        assert_eq!(reading.snapshot.windows[0].used_percent, 140.0);
    }

    #[test]
    fn every_output_bound_is_enforced() {
        let many = |count: usize| {
            let items: String = (0..count)
                .map(|index| format!("{{ id = \"m{index}\", title = \"M\" }},"))
                .collect();
            format!("return {{ metrics = {{{items}}}, card = {{}}, details = {{}} }}")
        };
        assert!(reading(&many(limits::MAX_METRICS)).is_ok());
        assert!(matches!(
            reading(&many(limits::MAX_METRICS + 1)),
            Err(PluginError::Output { .. })
        ));

        let long_id = format!(
            r#"return {{ metrics = {{ {{ id = "{}", title = "A" }} }}, card = {{}}, details = {{}} }}"#,
            "a".repeat(limits::MAX_ID_BYTES + 1)
        );
        assert!(matches!(reading(&long_id), Err(PluginError::Output { .. })));

        let long_text = format!(
            r#"return {{ metrics = {{ {{ id = "a", title = "A", text = "{}" }} }}, card = {{}}, details = {{}} }}"#,
            "t".repeat(limits::MAX_TEXT_BYTES + 1)
        );
        assert!(matches!(
            reading(&long_text),
            Err(PluginError::Output { .. })
        ));
    }

    #[test]
    fn the_snapshot_is_filed_under_the_account_being_polled() {
        let reading = reading(ONE_METRIC).expect("validates");
        assert_eq!(reading.snapshot.provider.as_str(), "com.acme.quota");
        assert_eq!(reading.snapshot.account.as_str(), "default");
        assert_eq!(reading.snapshot.captured_at.as_unix(), AT);
    }
}
