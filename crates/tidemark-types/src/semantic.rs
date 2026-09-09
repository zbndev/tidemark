//! How a reading is laid out, shared by the process that produces it and the ones that draw it.
//!
//! Presentation, and semantic on purpose: a producer names *what a number means* — a gauge
//! of a percentage, a value, an `X of Y` — and never a pixel, a colour or a font. That is
//! what lets one GTK renderer serve both a built-in adapter and an untrusted plugin, and
//! what lets a future Waybar module render the same reading without importing either.

use zvariant::{DeserializeDict, SerializeDict, Type};

/// One measurement, independent of how it is drawn.
#[derive(Debug, Clone, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct Metric {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub value: Option<f64>,
    pub maximum: Option<f64>,
    pub remaining: Option<f64>,
    pub used_percent: Option<f64>,
    pub text: Option<String>,
    pub unit: Option<String>,
    pub window: Option<MetricWindow>,
}

#[derive(Debug, Clone, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct MetricWindow {
    pub key: String,
    pub resets_at: Option<i64>,
    pub length_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetKind {
    Gauge,
    Value,
    Ratio,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Value,
    Maximum,
    Remaining,
    UsedPercent,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Number,
    Percent,
    Currency,
    Duration,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    Normal,
    Compact,
}

macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        impl $name {
            pub const fn as_wire(self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }
            pub fn from_wire(value: &str) -> Option<Self> {
                [$(Self::$variant),+].into_iter().find(|c| c.as_wire() == value)
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_wire())
            }
        }
    };
}

wire_enum!(WidgetKind { Gauge => "gauge", Value => "value", Ratio => "ratio", Status => "status" });
wire_enum!(Field { Value => "value", Maximum => "maximum", Remaining => "remaining", UsedPercent => "used_percent", Text => "text" });
wire_enum!(Format { Number => "number", Percent => "percent", Currency => "currency", Duration => "duration", Text => "text" });
wire_enum!(Emphasis { Normal => "normal", Compact => "compact" });

#[derive(Debug, Clone, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct Widget {
    pub kind: String,
    pub metric: String,
    pub field: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
    pub format: Option<String>,
    pub emphasis: Option<String>,
}

#[derive(Debug, Clone, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct PresentedSection {
    pub title: String,
    pub items: Vec<Widget>,
}

#[derive(Debug, Clone, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct Presentation {
    pub metrics: Vec<Metric>,
    pub card: Vec<Widget>,
    pub details: Vec<PresentedSection>,
}

impl Metric {
    pub fn field(&self, field: Field) -> Option<f64> {
        match field {
            Field::Value => self.value,
            Field::Maximum => self.maximum,
            Field::Remaining => self.remaining,
            Field::UsedPercent => self.used_percent,
            Field::Text => None,
        }
        .filter(|number| number.is_finite())
    }
}

impl Widget {
    pub fn gauge(metric: &str, field: Field) -> Self {
        Self::of(WidgetKind::Gauge, metric).with_field(field)
    }
    pub fn value(metric: &str, field: Field) -> Self {
        Self::of(WidgetKind::Value, metric).with_field(field)
    }
    pub fn status(metric: &str) -> Self {
        Self::of(WidgetKind::Status, metric).with_field(Field::Text)
    }
    pub fn ratio(metric: &str, left: Field, right: Field) -> Self {
        let mut widget = Self::of(WidgetKind::Ratio, metric);
        widget.left = Some(left.as_wire().to_owned());
        widget.right = Some(right.as_wire().to_owned());
        widget
    }
    fn of(kind: WidgetKind, metric: &str) -> Self {
        Self {
            kind: kind.as_wire().to_owned(),
            metric: metric.to_owned(),
            field: None,
            left: None,
            right: None,
            format: None,
            emphasis: None,
        }
    }
    fn with_field(mut self, field: Field) -> Self {
        self.field = Some(field.as_wire().to_owned());
        self
    }
    pub fn formatted(mut self, format: Format) -> Self {
        self.format = Some(format.as_wire().to_owned());
        self
    }
    pub fn emphasized(mut self, emphasis: Emphasis) -> Self {
        self.emphasis = Some(emphasis.as_wire().to_owned());
        self
    }
    pub fn kind(&self) -> Option<WidgetKind> {
        WidgetKind::from_wire(&self.kind)
    }
    pub fn field(&self) -> Option<Field> {
        Field::from_wire(self.field.as_deref()?)
    }
    pub fn left(&self) -> Option<Field> {
        Field::from_wire(self.left.as_deref()?)
    }
    pub fn right(&self) -> Option<Field> {
        Field::from_wire(self.right.as_deref()?)
    }
    pub fn format(&self) -> Option<Format> {
        Format::from_wire(self.format.as_deref()?)
    }
    pub fn emphasis(&self) -> Option<Emphasis> {
        Emphasis::from_wire(self.emphasis.as_deref()?)
    }
}

impl Presentation {
    pub fn metric(&self, id: &str) -> Option<&Metric> {
        self.metrics.iter().find(|metric| metric.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zvariant::serialized::Context;
    use zvariant::{LE, to_bytes};

    fn metric() -> Metric {
        Metric {
            id: "month-total-cost".into(),
            title: "Monthly cost".into(),
            subtitle: None,
            value: Some(12.5),
            maximum: Some(50.0),
            remaining: Some(37.5),
            used_percent: Some(25.0),
            text: None,
            unit: Some("USD".into()),
            window: None,
        }
    }

    #[test]
    fn a_widget_kind_travels_as_its_documented_string() {
        assert_eq!(WidgetKind::Gauge.as_wire(), "gauge");
        assert_eq!(WidgetKind::from_wire("ratio"), Some(WidgetKind::Ratio));
        assert_eq!(WidgetKind::from_wire("hologram"), None);
    }

    #[test]
    fn a_field_reference_reads_the_number_it_names_and_nothing_else() {
        let m = metric();
        assert_eq!(m.field(Field::Value), Some(12.5));
        assert_eq!(m.field(Field::UsedPercent), Some(25.0));
        let bare = Metric {
            value: None,
            ..metric()
        };
        assert_eq!(bare.field(Field::Value), None, "absent stays absent");
    }

    #[test]
    fn a_presentation_resolves_a_widgets_metric_by_id() {
        let p = Presentation {
            metrics: vec![metric()],
            card: vec![Widget::gauge("month-total-cost", Field::UsedPercent)],
            details: Vec::new(),
        };
        assert!(p.metric("month-total-cost").is_some());
        assert!(p.metric("no-such-metric").is_none());
    }

    #[test]
    fn the_published_shape_encodes_as_a_dictionary() {
        let p = Presentation {
            metrics: vec![metric()],
            card: vec![Widget::gauge("month-total-cost", Field::UsedPercent)],
            details: vec![PresentedSection {
                title: "Limits".into(),
                items: vec![Widget::ratio(
                    "month-total-cost",
                    Field::Value,
                    Field::Maximum,
                )],
            }],
        };
        to_bytes(Context::new_dbus(LE, 0), &p).expect("the published shape encodes");
    }

    #[test]
    fn an_absent_optional_field_is_absent_from_the_encoded_dictionary() {
        let encoded = to_bytes(Context::new_dbus(LE, 0), &metric()).expect("encodes");
        let decoded: std::collections::HashMap<String, zvariant::OwnedValue> =
            encoded.deserialize().expect("decodes").0;
        assert!(
            !decoded.contains_key("text"),
            "a metric with no text sends no text key"
        );
        assert!(decoded.contains_key("used_percent"));
    }
}
