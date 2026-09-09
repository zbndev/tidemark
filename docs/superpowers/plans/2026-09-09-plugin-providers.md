# Plugin Providers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a user import one untrusted `.tidemark-provider` file — TOML metadata, a sandboxed Lua 5.4 transform, an optional sanitized SVG — enter their own endpoint URL and API key, and get a card with the same polling, history, pace and notification behavior as a built-in provider.

**Architecture:** Two movements. First a *presentation cutover*: a semantic metric/widget vocabulary is added to `tidemark-types`, built-in providers are mapped onto it by one Rust adapter in `tidemark-core`, and the GTK card is rewritten to render only that vocabulary — so there is one renderer before any plugin exists. Then the *plugin path*: core owns the file schema, the SVG sanitizer, the Lua sandbox, the typed output validation and one generic single-request `Provider`; the daemon owns installed definitions, per-account endpoints, the keyring, dynamic registry entries and D-Bus; the GUI and `tidemarkctl` only import, configure and display.

**Tech Stack:** Rust 2024 (MSRV 1.92), `mlua` 0.12 with vendored Lua 5.4, `quick-xml` for the SVG sanitizer, `toml_edit` (already used) for the plugin file and config, `reqwest`/rustls for the one request, `zbus` 5 `a{sv}` dictionaries, GTK 4.22 + libadwaita 1.9.

**Spec:** `docs/superpowers/specs/2026-09-08-plugin-providers-design.md` — read it alongside this plan; every task argues from it.

## Global Constraints

- Layering (`scripts/check-layering.sh`) is law: `tidemark-types` gets no runtime I/O and no `zbus`; `tidemark-ipc` gets no policy; `tidemark-cli` gets no `tidemark-core`, HTTP, SQLite, GTK or Tokio; `tidemark-core` gets no GTK/GDK/adwaita; `tidemark` gets no core, HTTP or SQLite. **The Lua runtime, the TOML schema and the SVG sanitizer therefore live in `tidemark-core` only, and reach the CLI exclusively through daemon D-Bus methods.**
- Errors are contextual `thiserror` enums. Never `anyhow`.
- Missing provider values stay missing. No fabricated zero, maximum, percentage or reset timestamp — anywhere, including the plugin runtime and the built-in adapter.
- Published dictionaries are `a{sv}`: absent stays absent, and adding a key is not a breaking change.
- Provider ids and account ids are persistent storage keys. Never rename, never migrate silently.
- Cargo only: add dependencies with `cargo add`, never hand-edit `Cargo.lock`. No `[workspace.dependencies]` — each crate declares its own.
- Tests use `#[test]`/`#[tokio::test]`, mostly colocated; `History::in_memory()` and `FakeSecrets`, never the real keyring; deterministic timestamps, never `Timestamp::now()` in an assertion.
- `unsafe_code = "deny"`. Clippy: `-D warnings`, and `todo!`/`dbg!` are denied.
- **File format:** `format_version = 1` exactly; `provider.id` matches `[a-z0-9]+(?:[.-][a-z0-9]+)*` and contains at least one dot; built-in provider ids and the `tidemark.*` namespace are reserved.
- **Resource limits, verbatim (one module, one place):** plugin file 512 KiB; Lua source 128 KiB; HTTP body 4 MiB before JSON parsing; Lua heap 16 MiB per execution; 1,000,000 VM instructions per execution; 128 metrics; 32 card items; 32 detail sections with at most 128 items each.
- **Request boundary:** `GET` or empty-body `POST` only; the account owner supplies the whole absolute URL; redirects disabled; URL credentials and fragments rejected; plain HTTP needs a per-account acknowledgement; refused header names: `Host`, `Cookie`, `Connection`, `Content-Length`, `Transfer-Encoding`, `Proxy-Authorization`, `Proxy-Connection`, `TE`, `Trailer`, `Upgrade`; Tidemark's own `User-Agent` (`Tidemark/<version>`) and `Accept: application/json` cannot be overridden.
- **The key never reaches Lua.** Not in `response`, not in `context`, not in a diagnostic, not in the debug recorder.
- **Size budget:** the stripped Linux release `tidemarkd` may grow at most 1.5 MiB against the pre-feature commit. Measured in Task 19; over budget returns to design review rather than being accepted.
- Local gate for every task: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh`.
- Commit messages end with the session's attribution lines.

## Phase boundary

Tasks 1–4 are the presentation cutover and are shippable on their own: after Task 4 every built-in card renders through the new shared vocabulary and nothing about plugins exists yet. That is the natural merge point if this branch is split in two.

## File Structure

**`tidemark-types`**
- Create `crates/tidemark-types/src/semantic.rs` — `Metric`, `MetricWindow`, `Widget`, `WidgetKind`, `Field`, `Format`, `Emphasis`, `PresentedSection`, `Presentation`. Wire vocabulary only: no parsing, no policy.
- Modify `crates/tidemark-types/src/snapshot.rs` — add `ordered_windows`, the single spelling of card window order.
- Modify `crates/tidemark-types/src/present.rs` — add `plugin_icon_slug`, `field_value`, `format_metric`.
- Modify `crates/tidemark-types/src/wire.rs` — `ProviderStatus::presentation`.

**`tidemark-core`** (all plugin machinery)
- Create `crates/tidemark-core/src/presentation.rs` — the built-in `Snapshot` → `Presentation` adapter.
- Create `crates/tidemark-core/src/plugin/mod.rs` — `Definition`, `PluginError`, the public entry points.
- Create `crates/tidemark-core/src/plugin/limits.rs` — every constant from Global Constraints, and nothing else.
- Create `crates/tidemark-core/src/plugin/schema.rs` — `.tidemark-provider` TOML parsing and metadata/request validation.
- Create `crates/tidemark-core/src/plugin/svg.rs` — the XML sanitizer.
- Create `crates/tidemark-core/src/plugin/lua/mod.rs` — sandbox construction and `run`.
- Create `crates/tidemark-core/src/plugin/lua/json.rs` — JSON ⇄ Lua with the null sentinel.
- Create `crates/tidemark-core/src/plugin/lua/api.rs` — the host globals.
- Create `crates/tidemark-core/src/plugin/output.rs` — Lua return value → `(Snapshot, Presentation)`.
- Create `crates/tidemark-core/src/plugin/provider.rs` — the generic single-request `Provider`.
- Modify `crates/tidemark-core/src/config.rs` — account-addressed endpoint read/write/remove.
- Modify `crates/tidemark-core/src/debug.rs` — redact an exact key occurrence in a recorded body.

**`tidemarkd`**
- Create `crates/tidemarkd/src/plugins.rs` — the installed-definition store on disk.
- Modify `crates/tidemarkd/src/registry.rs`, `engine.rs`, `service.rs`.

**`tidemark-ipc`** — modify `src/lib.rs` (five methods, one signal).

**`tidemark`** — modify `src/card.rs`, `src/detail.rs`, `src/mark.rs`, `src/model.rs`; create `src/provider_settings/plugins.rs`.

**`tidemark-cli`** — modify `src/cli.rs`, `src/commands/mod.rs`; create `src/commands/plugin.rs`.

**Docs and assets** — create `docs/plugin-providers.md`, `examples/plugins/acme-quota.tidemark-provider`, `crates/tidemark-core/tests/fixtures/plugin/acme-response.json`; modify `README.md`.

---

### Task 1: Shared semantic presentation vocabulary

**Files:**
- Create: `crates/tidemark-types/src/semantic.rs`
- Modify: `crates/tidemark-types/src/lib.rs` (module + re-exports)
- Modify: `crates/tidemark-types/src/snapshot.rs` (add `ordered_windows`)
- Modify: `crates/tidemark-types/src/present.rs` (add `plugin_icon_slug`)
- Test: colocated in each file

**Interfaces:**
- Consumes: nothing.
- Produces: `Metric`, `MetricWindow`, `Widget`, `PresentedSection`, `Presentation`, `WidgetKind`, `Field`, `Format`, `Emphasis`, `Presentation::metric(&str) -> Option<&Metric>`, `Metric::field(Field) -> Option<f64>`, `snapshot::ordered_windows(&Snapshot) -> Vec<Window>`, `present::plugin_icon_slug(&str) -> Option<String>`.

- [ ] **Step 1: Write the failing tests**

Create `crates/tidemark-types/src/semantic.rs` with only its `mod tests` filled in first:

```rust
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
        let bare = Metric { value: None, ..metric() };
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
                items: vec![Widget::ratio("month-total-cost", Field::Value, Field::Maximum)],
            }],
        };
        to_bytes(Context::new_dbus(LE, 0), &p).expect("the published shape encodes");
    }

    #[test]
    fn an_absent_optional_field_is_absent_from_the_encoded_dictionary() {
        let encoded = to_bytes(Context::new_dbus(LE, 0), &metric()).expect("encodes");
        let decoded: std::collections::HashMap<String, zvariant::OwnedValue> =
            encoded.deserialize().expect("decodes").0;
        assert!(!decoded.contains_key("text"), "a metric with no text sends no text key");
        assert!(decoded.contains_key("used_percent"));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tidemark-types semantic`
Expected: FAIL — `cannot find type Metric in this scope`.

- [ ] **Step 3: Write the vocabulary**

At the top of `crates/tidemark-types/src/semantic.rs`:

```rust
//! How a reading is laid out, shared by the process that produces it and the ones that draw it.
//!
//! Presentation, and semantic on purpose: a producer names *what a number means* — a gauge
//! of a percentage, a value, an `X of Y` — and never a pixel, a colour or a font. That is
//! what lets one GTK renderer serve both a built-in adapter and an untrusted plugin, and
//! what lets a future Waybar module render the same reading without importing either.

use serde::{Deserialize, Serialize};
use zvariant::{DeserializeDict, SerializeDict, Type};

/// One measurement, independent of how it is drawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct Metric {
    /// Stable identity within one reading. History identity for a windowed metric.
    pub id: String,
    /// Plain display label.
    pub title: String,
    /// A second line, when the producer has one.
    pub subtitle: Option<String>,
    /// The quantity consumed, in `unit`.
    pub value: Option<f64>,
    /// The quantity available.
    pub maximum: Option<f64>,
    /// What is left.
    pub remaining: Option<f64>,
    /// Consumption in percent. Values above 100 are truthful data, not an error.
    pub used_percent: Option<f64>,
    /// A non-numeric status.
    pub text: Option<String>,
    /// A documented unit or a bounded plain suffix.
    pub unit: Option<String>,
    /// Window metadata, for a metric that participates in history, pace and notifications.
    pub window: Option<MetricWindow>,
}

/// What makes a metric a rate-limit window rather than a number on a card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct MetricWindow {
    /// Stable window identity — see `crate::WindowKey`.
    pub key: String,
    /// Unix seconds of the next rollover, when the producer said.
    pub resets_at: Option<i64>,
    /// Length in seconds, when the producer said.
    pub length_secs: Option<u64>,
}

/// One of the four semantic widgets. Travels as a string so a newer producer can add one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetKind {
    /// A bar plus the selected number.
    Gauge,
    /// One number or one piece of text.
    Value,
    /// Two numbers as `X of Y`.
    Ratio,
    /// Bounded plain text in one of the semantic states.
    Status,
}

/// Which number of a metric a widget reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Value,
    Maximum,
    Remaining,
    UsedPercent,
    /// The metric's `text`, for a `status` or a textual `value`.
    Text,
}

/// How a number is spelled. Never a format string: the shared functions in
/// [`crate::present`] do the spelling, so the GUI and the CLI agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Number,
    Percent,
    Currency,
    Duration,
    Text,
}

/// How much room a widget gets. Not a size: the renderer owns the geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    /// The card's leading reading.
    Normal,
    /// A secondary row.
    Compact,
}

macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        impl $name {
            /// The stable string this value travels as.
            pub const fn as_wire(self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }
            /// Parses it. `None` for anything this build does not know.
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
wire_enum!(Field {
    Value => "value",
    Maximum => "maximum",
    Remaining => "remaining",
    UsedPercent => "used_percent",
    Text => "text",
});
wire_enum!(Format {
    Number => "number",
    Percent => "percent",
    Currency => "currency",
    Duration => "duration",
    Text => "text",
});
wire_enum!(Emphasis { Normal => "normal", Compact => "compact" });

/// One placement of one metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct Widget {
    /// A [`WidgetKind`] as a string.
    pub kind: String,
    /// The [`Metric::id`] this draws.
    pub metric: String,
    /// The [`Field`] a gauge, value or status reads.
    pub field: Option<String>,
    /// A ratio's numerator field.
    pub left: Option<String>,
    /// A ratio's denominator field.
    pub right: Option<String>,
    /// A [`Format`], when the producer chose one.
    pub format: Option<String>,
    /// An [`Emphasis`], when the producer chose one.
    pub emphasis: Option<String>,
}

/// A titled, ordered group of widgets in the detail dialog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct PresentedSection {
    pub title: String,
    pub items: Vec<Widget>,
}

/// Everything one reading looks like: the numbers, then two orders over them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct Presentation {
    pub metrics: Vec<Metric>,
    /// Card widgets, in exactly the producer's order.
    pub card: Vec<Widget>,
    /// Detail sections, in exactly the producer's order.
    pub details: Vec<PresentedSection>,
}

impl Metric {
    /// The number a field names, or `None` when the producer did not report it.
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
    /// A gauge over one field.
    pub fn gauge(metric: &str, field: Field) -> Self {
        Self::of(WidgetKind::Gauge, metric).with_field(field)
    }
    /// A standalone value.
    pub fn value(metric: &str, field: Field) -> Self {
        Self::of(WidgetKind::Value, metric).with_field(field)
    }
    /// Plain status text.
    pub fn status(metric: &str) -> Self {
        Self::of(WidgetKind::Status, metric).with_field(Field::Text)
    }
    /// `X of Y`.
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

    /// Sets the spelling.
    pub fn formatted(mut self, format: Format) -> Self {
        self.format = Some(format.as_wire().to_owned());
        self
    }

    /// Sets the emphasis.
    pub fn emphasized(mut self, emphasis: Emphasis) -> Self {
        self.emphasis = Some(emphasis.as_wire().to_owned());
        self
    }

    /// The kind, or `None` for a widget this build cannot draw.
    pub fn kind(&self) -> Option<WidgetKind> {
        WidgetKind::from_wire(&self.kind)
    }
    /// The field, when there is one this build knows.
    pub fn field(&self) -> Option<Field> {
        Field::from_wire(self.field.as_deref()?)
    }
    /// A ratio's numerator field.
    pub fn left(&self) -> Option<Field> {
        Field::from_wire(self.left.as_deref()?)
    }
    /// A ratio's denominator field.
    pub fn right(&self) -> Option<Field> {
        Field::from_wire(self.right.as_deref()?)
    }
    /// The chosen spelling, when there is one.
    pub fn format(&self) -> Option<Format> {
        Format::from_wire(self.format.as_deref()?)
    }
    /// The chosen emphasis, when there is one.
    pub fn emphasis(&self) -> Option<Emphasis> {
        Emphasis::from_wire(self.emphasis.as_deref()?)
    }
}

impl Presentation {
    /// The metric a widget names, or `None` for a dangling reference.
    pub fn metric(&self, id: &str) -> Option<&Metric> {
        self.metrics.iter().find(|metric| metric.id == id)
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p tidemark-types semantic`
Expected: PASS (5 tests).

- [ ] **Step 5: Move the card window order into the shared crate**

Add to `crates/tidemark-types/src/snapshot.rs`, after `dominant_window`:

```rust
/// The order a card draws a reading's windows in: the lead window first, then shortest
/// first, then the windows whose length the provider never said.
///
/// One spelling, in the shared crate, because two producers need it — the GUI drew this
/// order before the semantic presentation existed, and the built-in adapter now has to
/// emit exactly the same sequence or the cutover would silently rearrange every card.
pub fn ordered_windows(snapshot: &Snapshot) -> Vec<Window> {
    let lead = snapshot.dominant_window().map(|window| window.key.clone());
    let mut windows = snapshot.windows.clone();
    windows.sort_by_key(|window| {
        (
            !lead.as_ref().is_some_and(|key| *key == window.key),
            window.length.is_none(),
            window.length.map(WindowLength::as_secs),
        )
    });
    windows
}
```

Add its test in the same file's `mod tests`:

```rust
    #[test]
    fn the_card_order_leads_with_the_dominant_window_then_shortest_first() {
        let s = snapshot(&[Some(2_592_000), None, Some(18_000), Some(604_800)]);
        let lengths: Vec<Option<u64>> = ordered_windows(&s)
            .iter()
            .map(|w| w.length.map(WindowLength::as_secs))
            .collect();
        assert_eq!(lengths, [Some(18_000), Some(604_800), Some(2_592_000), None]);
    }
```

- [ ] **Step 6: Add the plugin mark slug**

Add to `crates/tidemark-types/src/present.rs`:

```rust
/// The icon-theme slug an installed plugin's mark is filed under.
///
/// A plugin id is reverse-DNS and contains dots; [`icon_name`] refuses a dot, because the
/// installed built-in marks never have one. Dots become hyphens here, and the result goes
/// through [`icon_name`] unchanged — so a plugin mark is looked up by exactly the mechanism
/// a built-in mark is, and a malformed id names no icon at all rather than a path.
pub fn plugin_icon_slug(provider_id: &str) -> Option<String> {
    let usable = !provider_id.is_empty()
        && provider_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-');
    usable.then(|| provider_id.replace('.', "-"))
}
```

And its test:

```rust
    #[test]
    fn a_plugin_id_names_a_mark_through_the_same_lookup_a_built_in_does() {
        let slug = plugin_icon_slug("com.acme.quota").expect("usable");
        assert_eq!(slug, "com-acme-quota");
        assert_eq!(icon_name(&slug).as_deref(), Some("tidemark-com-acme-quota-symbolic"));
        assert_eq!(plugin_icon_slug("../../etc/passwd"), None);
        assert_eq!(plugin_icon_slug("Com.Acme"), None);
    }
```

- [ ] **Step 7: Export the new vocabulary**

In `crates/tidemark-types/src/lib.rs`: add `pub mod semantic;`, extend the `present` re-export to `pub use present::{duration, icon_name, percent, plugin_icon_slug};`, extend the `snapshot` re-export with `ordered_windows`, and add:

```rust
pub use semantic::{
    Emphasis, Field, Format, Metric, MetricWindow, PresentedSection, Presentation, Widget,
    WidgetKind,
};
```

- [ ] **Step 8: Point the GUI at the shared order**

In `crates/tidemark/src/model.rs`, delete the body of `ordered_windows` and re-export the shared one:

```rust
/// The order a card draws a reading's windows in. Defined in the shared crate, so the
/// daemon's adapter and this renderer cannot drift apart.
pub use tidemark_types::ordered_windows;
```

Keep the existing test in `model.rs` that asserts `ordered[0] == snapshot.dominant_window()` — it now guards the shared function from this side too.

- [ ] **Step 9: Run the gate**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh`
Expected: all pass. `check-layering.sh` prints `layering ok` (the new module adds no dependency).

- [ ] **Step 10: Commit**

```bash
git add crates/tidemark-types crates/tidemark/src/model.rs
git commit -m "feat(types): semantic presentation vocabulary"
```

---

### Task 2: The built-in adapter

**Files:**
- Create: `crates/tidemark-core/src/presentation.rs`
- Modify: `crates/tidemark-core/src/lib.rs` (`pub mod presentation;`)
- Test: colocated

**Interfaces:**
- Consumes: Task 1's `Presentation`, `Metric`, `Widget`, `Field`, `Emphasis`, `Format`, `MetricWindow`, `ordered_windows`.
- Produces: `tidemark_core::presentation::from_snapshot(snapshot: &Snapshot) -> Presentation`.

- [ ] **Step 1: Write the failing tests**

Create `crates/tidemark-core/src/presentation.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{
        AccountId, DetailRow, DetailSection, Emphasis, Field, ProviderId, Snapshot, Timestamp,
        Window, WindowKey, WindowLength,
    };

    fn at() -> Timestamp {
        Timestamp::from_unix(1_788_870_896).expect("plausible")
    }

    fn window(key: &str, length: u64, used: f64, subtitle: Option<&str>) -> Window {
        Window {
            key: WindowKey::named(key),
            title: format!("{key} title"),
            subtitle: subtitle.map(str::to_owned),
            used_percent: used,
            resets_at: Some(at().saturating_add_seconds(600)),
            length: WindowLength::from_secs(length),
        }
    }

    fn snapshot(windows: Vec<Window>, details: Vec<DetailSection>) -> Snapshot {
        Snapshot {
            provider: ProviderId::new("zai"),
            account: AccountId::default(),
            captured_at: at(),
            windows,
            details,
        }
    }

    #[test]
    fn each_window_becomes_a_gauge_in_card_order() {
        let p = from_snapshot(&snapshot(
            vec![
                window("w604800", 604_800, 10.0, None),
                window("w18000", 18_000, 42.0, Some("100 / 1000 credits")),
            ],
            Vec::new(),
        ));
        let metrics: Vec<&str> = p.card.iter().map(|w| w.metric.as_str()).collect();
        assert_eq!(metrics, ["w18000", "w604800"], "shortest leads, as the card always did");
        assert_eq!(p.card[0].kind, "gauge");
        assert_eq!(p.card[0].field(), Some(Field::UsedPercent));
        assert_eq!(p.card[0].emphasis(), Some(Emphasis::Normal));
        assert_eq!(p.card[1].emphasis(), Some(Emphasis::Compact));
    }

    #[test]
    fn a_windows_identity_and_metadata_survive_the_mapping() {
        let p = from_snapshot(&snapshot(vec![window("w18000", 18_000, 42.0, Some("1 / 2"))], vec![]));
        let metric = p.metric("w18000").expect("present");
        assert_eq!(metric.used_percent, Some(42.0));
        assert_eq!(metric.subtitle.as_deref(), Some("1 / 2"));
        let w = metric.window.as_ref().expect("a window metric carries its window");
        assert_eq!(w.key, "w18000");
        assert_eq!(w.length_secs, Some(18_000));
        assert_eq!(w.resets_at, Some(at().saturating_add_seconds(600).as_unix()));
    }

    #[test]
    fn a_window_without_a_reset_or_length_carries_neither() {
        let mut bare = window("monthly", 60, 5.0, None);
        bare.resets_at = None;
        bare.length = None;
        let p = from_snapshot(&snapshot(vec![bare], vec![]));
        let w = p.metric("monthly").expect("present").window.clone().expect("windowed");
        assert_eq!(w.resets_at, None, "nothing is invented for a reset the provider never sent");
        assert_eq!(w.length_secs, None);
    }

    #[test]
    fn detail_rows_become_status_widgets_in_their_original_order() {
        let p = from_snapshot(&snapshot(
            vec![],
            vec![DetailSection {
                title: "Plan".into(),
                rows: vec![
                    DetailRow { label: "Level".into(), value: "pro".into() },
                    DetailRow { label: "Seats".into(), value: "3".into() },
                ],
            }],
        ));
        let section = &p.details[0];
        assert_eq!(section.title, "Plan");
        let labels: Vec<&str> = section
            .items
            .iter()
            .map(|item| p.metric(&item.metric).expect("present").title.as_str())
            .collect();
        assert_eq!(labels, ["Level", "Seats"]);
        assert_eq!(section.items[0].kind, "status");
        let first = p.metric(&section.items[0].metric).expect("present");
        assert_eq!(first.text.as_deref(), Some("pro"));
        assert!(first.window.is_none(), "a detail row is not a window and never enters history");
    }

    #[test]
    fn detail_metric_ids_do_not_collide_with_window_ids_or_each_other() {
        let p = from_snapshot(&snapshot(
            vec![window("w18000", 18_000, 1.0, None)],
            vec![
                DetailSection { title: "Plan".into(), rows: vec![DetailRow { label: "w18000".into(), value: "a".into() }] },
                DetailSection { title: "Other".into(), rows: vec![DetailRow { label: "w18000".into(), value: "b".into() }] },
            ],
        ));
        let mut ids: Vec<&str> = p.metrics.iter().map(|m| m.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "every metric id in one reading is unique");
    }

    #[test]
    fn a_reading_with_nothing_in_it_presents_nothing_rather_than_a_placeholder() {
        let p = from_snapshot(&snapshot(vec![], vec![]));
        assert!(p.metrics.is_empty() && p.card.is_empty() && p.details.is_empty());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tidemark-core presentation`
Expected: FAIL — `cannot find function from_snapshot`.

- [ ] **Step 3: Write the adapter**

Above the tests in the same file:

```rust
//! Every built-in provider's reading, in the semantic presentation the card renders.
//!
//! One adapter, in core, so the plugin path and the built-in path reach the GTK card
//! through the same shape and there is no second renderer to keep in step. It is a pure
//! function of a [`Snapshot`]: the windows become gauges in the order the card has always
//! drawn them, and the detail sections become ordered status text. Nothing is invented —
//! a window with no reset time produces a metric with no reset time.

use tidemark_types::{
    DetailSection, Emphasis, Field, Metric, MetricWindow, Presentation, PresentedSection,
    Snapshot, Timestamp, Widget, Window, WindowLength, ordered_windows,
};

/// The presentation of one built-in reading.
pub fn from_snapshot(snapshot: &Snapshot) -> Presentation {
    let mut metrics = Vec::new();
    let mut card = Vec::new();

    for (index, window) in ordered_windows(snapshot).iter().enumerate() {
        metrics.push(window_metric(window));
        let emphasis = if index == 0 { Emphasis::Normal } else { Emphasis::Compact };
        card.push(
            Widget::gauge(window.key.as_str(), Field::UsedPercent)
                .formatted(tidemark_types::Format::Percent)
                .emphasized(emphasis),
        );
    }

    let mut details = Vec::with_capacity(snapshot.details.len());
    for (section_index, section) in snapshot.details.iter().enumerate() {
        let (section, rows) = detail_section(section_index, section);
        metrics.extend(rows);
        details.push(section);
    }

    Presentation { metrics, card, details }
}

/// One window as a metric. `id` is the window key, so the metric's identity and the
/// history's identity are the same string rather than two strings that agree today.
fn window_metric(window: &Window) -> Metric {
    Metric {
        id: window.key.to_string(),
        title: window.title.clone(),
        subtitle: window.subtitle.clone(),
        value: None,
        maximum: None,
        remaining: None,
        used_percent: Some(window.used_percent),
        text: None,
        unit: None,
        window: Some(MetricWindow {
            key: window.key.to_string(),
            resets_at: window.resets_at.map(Timestamp::as_unix),
            length_secs: window.length.map(WindowLength::as_secs),
        }),
    }
}

/// One detail section, with a metric per row.
///
/// The ids are positional — `detail/<section>/<row>` — because a row's label is the
/// provider's prose and two sections may legitimately use the same one, and a duplicate id
/// is the one thing the validated model refuses.
fn detail_section(section_index: usize, section: &DetailSection) -> (PresentedSection, Vec<Metric>) {
    let mut metrics = Vec::with_capacity(section.rows.len());
    let mut items = Vec::with_capacity(section.rows.len());
    for (row_index, row) in section.rows.iter().enumerate() {
        let id = format!("detail/{section_index}/{row_index}");
        metrics.push(Metric {
            id: id.clone(),
            title: row.label.clone(),
            subtitle: None,
            value: None,
            maximum: None,
            remaining: None,
            used_percent: None,
            text: Some(row.value.clone()),
            unit: None,
            window: None,
        });
        items.push(Widget::status(&id));
    }
    (PresentedSection { title: section.title.clone(), items }, metrics)
}
```

Add `pub mod presentation;` to `crates/tidemark-core/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p tidemark-core presentation`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/tidemark-core/src/presentation.rs crates/tidemark-core/src/lib.rs
git commit -m "feat(core): map built-in readings onto the semantic presentation"
```

---

### Task 3: Publish the presentation

**Files:**
- Modify: `crates/tidemark-types/src/wire.rs` (`ProviderStatus::presentation`, `set_reading` signature)
- Modify: `crates/tidemarkd/src/engine.rs` (`apply`/`record` call site)
- Test: colocated in both

**Interfaces:**
- Consumes: Task 1's `Presentation`, Task 2's `from_snapshot`.
- Produces: `ProviderStatus.presentation: Option<Presentation>`, `ProviderStatus::set_reading(&mut self, snapshot: &Snapshot, presentation: Presentation)`.

- [ ] **Step 1: Write the failing test**

In `crates/tidemark-types/src/wire.rs`'s `mod tests`:

```rust
    #[test]
    fn a_published_reading_carries_its_presentation_and_survives_a_round_trip() {
        let status = status();
        let decoded: HashMap<String, OwnedValue> = encode(&status).deserialize().expect("decodes").0;
        assert!(decoded.contains_key("presentation"));
        let presentation = status.presentation.expect("the reading was published with one");
        assert_eq!(presentation.card.len(), 2, "one gauge per published window");
    }

    #[test]
    fn a_status_that_has_never_had_a_reading_publishes_no_presentation() {
        let pending = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        assert!(pending.presentation.is_none());
        let decoded: HashMap<String, OwnedValue> = encode(&pending).deserialize().expect("decodes").0;
        assert!(!decoded.contains_key("presentation"), "absent means absent");
    }
```

Update the test helper `fn status()` in that module to build the presentation by hand (the types crate cannot call core's adapter):

```rust
    fn status() -> ProviderStatus {
        let mut status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        // The same literal the helper already built, lifted into a local so the presentation
        // below can be derived from it rather than repeating its windows.
        let snapshot = Snapshot {
            provider: ProviderId::new("zai"),
            account: AccountId::default(),
            captured_at: Timestamp::from_unix(1_785_700_000).expect("plausible"),
            windows: vec![window(Some(1_785_717_000)), window(None)],
            details: vec![DetailSection {
                title: "Plan".into(),
                rows: vec![DetailRow { label: "Level".into(), value: "pro".into() }],
            }],
        };
        let presentation = Presentation {
            metrics: snapshot
                .windows
                .iter()
                .map(|window| Metric {
                    id: window.key.to_string(),
                    title: window.title.clone(),
                    subtitle: None,
                    value: None,
                    maximum: None,
                    remaining: None,
                    used_percent: Some(window.used_percent),
                    text: None,
                    unit: None,
                    window: None,
                })
                .collect(),
            card: snapshot
                .windows
                .iter()
                .map(|window| Widget::gauge(window.key.as_str(), Field::UsedPercent))
                .collect(),
            details: Vec::new(),
        };
        status.set_reading(&snapshot, presentation);
        status.next_poll_at = Some(1_785_700_300);
        status
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tidemark-types wire`
Expected: FAIL — `no field presentation on type ProviderStatus`.

- [ ] **Step 3: Add the field and change the setter**

In `crates/tidemark-types/src/wire.rs`, add to `ProviderStatus` after `details`:

```rust
    /// How the last good reading is laid out: its metrics, and the card and detail orders
    /// over them. **Survives a failed poll**, exactly as `windows` does.
    ///
    /// Absent while the account has never been polled successfully, and absent from a
    /// daemon older than the semantic presentation — which a client must draw as "an older
    /// daemon", never as "a reading with nothing in it".
    pub presentation: Option<Presentation>,
```

Add `presentation: None` to `ProviderStatus::pending`. Change `set_reading`:

```rust
    /// Replaces the reading and its layout, and sets the state to [`ProviderState::Ok`].
    ///
    /// The presentation is a parameter rather than something derived here: deriving it
    /// would put provider conventions in the contract crate, and a plugin's layout is not
    /// derivable from a `Snapshot` at all.
    pub fn set_reading(&mut self, snapshot: &Snapshot, presentation: Presentation) {
        self.captured_at = Some(snapshot.captured_at.as_unix());
        self.windows = snapshot.windows.iter().map(WindowStatus::from_window).collect();
        self.details = snapshot.details.clone();
        self.presentation = Some(presentation);
        self.set_state(ProviderState::Ok, None);
    }
```

Import `crate::semantic::Presentation` at the top of the file.

- [ ] **Step 4: Update the one production call site**

In `crates/tidemarkd/src/engine.rs`, find the `set_reading` call inside `Engine::apply` and pass the adapter's result:

```rust
        let presentation = tidemark_core::presentation::from_snapshot(snapshot);
        self.accounts[index].status.set_reading(snapshot, presentation);
```

Fix every remaining compile error from the signature change by threading the adapter's output (test helpers in `service.rs`, `engine.rs`, `notify.rs` and the GUI/CLI test fixtures may all call it).

- [ ] **Step 5: Write the daemon-side test**

In `crates/tidemarkd/src/engine.rs`'s `mod tests`, beside the existing apply tests:

```rust
    #[tokio::test]
    async fn a_successful_poll_publishes_the_presentation_of_its_reading() {
        let mut engine = engine_with(vec![Ok(snapshot_with_windows(&[("w18000", 18_000, 42.0)]))]).await;
        engine.poll_due(Instant::now()).await;
        let presentation = engine.accounts()[0]
            .status()
            .presentation
            .clone()
            .expect("a successful poll publishes a layout");
        assert_eq!(presentation.card.len(), 1);
        assert_eq!(presentation.card[0].metric, "w18000");
    }

    #[tokio::test]
    async fn a_failed_poll_keeps_the_last_good_presentation() {
        let mut engine = engine_with(vec![
            Ok(snapshot_with_windows(&[("w18000", 18_000, 42.0)])),
            Err(ProviderError::Http { status: 500 }),
        ])
        .await;
        engine.poll_due(Instant::now()).await;
        engine.accounts_mut_mark_due();
        engine.poll_due(Instant::now()).await;
        let status = engine.accounts()[0].status();
        assert_eq!(status.state(), Some(ProviderState::Unreachable));
        assert!(
            status.presentation.is_some(),
            "the card keeps showing the last known numbers behind a failure chip"
        );
    }
```

Reuse the module's existing fake-provider and engine helpers; add `snapshot_with_windows` beside them if it does not exist, and use the module's existing mechanism for making an account due again rather than inventing `accounts_mut_mark_due` if one is already there.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p tidemark-types -p tidemarkd`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/tidemark-types/src/wire.rs crates/tidemarkd
git commit -m "feat(ipc): publish the semantic presentation with every reading"
```

---

### Task 4: The GTK card renders the presentation

**Files:**
- Modify: `crates/tidemark/src/card.rs` (`apply`, `rebuild_rows`)
- Modify: `crates/tidemark/src/detail.rs` (`rebuild_details`)
- Modify: `crates/tidemark-types/src/present.rs` (shared value spelling)
- Test: colocated

**Interfaces:**
- Consumes: `ProviderStatus.presentation`, `Widget`, `Metric`, `Field`, `Format`, `Emphasis`.
- Produces: `present::format_field(metric: &Metric, field: Field, format: Option<Format>) -> Option<String>`, `present::format_ratio(metric: &Metric, left: Field, right: Field) -> Option<String>`, and a `card.rs` renderer driven by `Presentation`.

- [ ] **Step 1: Write the failing shared-formatting tests**

In `crates/tidemark-types/src/present.rs`'s `mod tests`:

```rust
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
        assert_eq!(format_field(&m, Field::UsedPercent, Some(Format::Percent)).as_deref(), Some("25%"));
        assert_eq!(format_field(&m, Field::Value, Some(Format::Currency)).as_deref(), Some("12.50 USD"));
        assert_eq!(format_field(&m, Field::Text, Some(Format::Text)).as_deref(), Some("active"));
    }

    #[test]
    fn a_field_the_producer_never_reported_is_spelled_as_nothing() {
        use crate::{Field, Format};
        let bare = crate::Metric { remaining: None, ..metric() };
        assert_eq!(format_field(&bare, Field::Remaining, Some(Format::Number)), None);
    }

    #[test]
    fn a_ratio_needs_both_operands() {
        use crate::Field;
        let m = metric();
        assert_eq!(format_ratio(&m, Field::Value, Field::Maximum).as_deref(), Some("12.5 of 50 USD"));
        let bare = crate::Metric { maximum: None, ..metric() };
        assert_eq!(format_ratio(&bare, Field::Value, Field::Maximum), None);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemark-types present`
Expected: FAIL — `cannot find function format_field`.

- [ ] **Step 3: Implement the shared spelling**

Add to `crates/tidemark-types/src/present.rs`:

```rust
use crate::semantic::{Field, Format, Metric};

/// One field of a metric, spelled the way the producer asked for.
///
/// Shared rather than written in the card, for the reason the rest of this module is: the
/// CLI, the notification and the card must print one number one way, and a plugin author
/// picks a *semantic* format precisely so that the house style stays ours.
pub fn format_field(metric: &Metric, field: Field, format: Option<Format>) -> Option<String> {
    if field == Field::Text {
        return metric.text.clone();
    }
    let number = metric.field(field)?;
    let unit = metric.unit.as_deref();
    Some(match format.unwrap_or(Format::Number) {
        Format::Percent => percent(number),
        Format::Currency => match unit {
            Some(unit) => format!("{number:.2} {unit}"),
            None => format!("{number:.2}"),
        },
        Format::Duration => duration(number.round() as i64),
        Format::Text => trim_number(number),
        Format::Number => match unit {
            Some(unit) => format!("{} {unit}", trim_number(number)),
            None => trim_number(number),
        },
    })
}

/// Two fields as `X of Y`, in the metric's unit. `None` unless both are there.
pub fn format_ratio(metric: &Metric, left: Field, right: Field) -> Option<String> {
    let (left, right) = (metric.field(left)?, metric.field(right)?);
    let body = format!("{} of {}", trim_number(left), trim_number(right));
    Some(match metric.unit.as_deref() {
        Some(unit) => format!("{body} {unit}"),
        None => body,
    })
}

/// A number without a trailing `.0`: quotas are counts as often as they are amounts.
fn trim_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{number:.0}")
    } else {
        let rendered = format!("{number:.2}");
        rendered.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}
```

Export `format_field` and `format_ratio` from `lib.rs`'s `present` re-export.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p tidemark-types present`
Expected: PASS.

- [ ] **Step 5: Write the failing card test**

In `crates/tidemark/src/card.rs`'s `mod tests`:

```rust
    fn presented(card: Vec<Widget>, metrics: Vec<Metric>) -> ProviderStatus {
        let mut status = status_with(Vec::new());
        status.captured_at = Some(1_788_870_896);
        status.presentation = Some(Presentation { metrics, card, details: Vec::new() });
        status
    }

    #[test]
    fn the_card_draws_the_widgets_in_the_published_order() {
        let status = presented(
            vec![
                Widget::gauge("a", Field::UsedPercent),
                Widget::value("b", Field::Remaining),
                Widget::ratio("a", Field::Value, Field::Maximum),
            ],
            vec![numeric_metric("a"), numeric_metric("b")],
        );
        let rows = card_rows(&status);
        assert_eq!(
            rows.iter().map(|row| row.kind).collect::<Vec<_>>(),
            [WidgetKind::Gauge, WidgetKind::Value, WidgetKind::Ratio],
            "the producer's order is the card's order"
        );
    }

    #[test]
    fn a_widget_naming_a_metric_that_is_not_there_is_not_drawn() {
        let status = presented(vec![Widget::gauge("ghost", Field::UsedPercent)], vec![numeric_metric("a")]);
        assert!(card_rows(&status).is_empty(), "a dangling reference draws nothing");
    }

    #[test]
    fn a_card_from_an_older_daemon_still_draws_its_windows() {
        let mut status = status_with(vec![window(Some(18_000), 42.0)]);
        status.presentation = None;
        assert_eq!(card_rows(&status).len(), 1, "the compatibility path is the windows list");
    }
```

Add the two helpers used above beside them: `numeric_metric(id)` building a `Metric` with `value`/`maximum`/`remaining`/`used_percent` set, and `card_rows(&ProviderStatus) -> Vec<Row>` — the pure function extracted in the next step.

- [ ] **Step 6: Run to verify they fail**

Run: `cargo test -p tidemark card`
Expected: FAIL — `cannot find function card_rows`.

- [ ] **Step 7: Render the presentation**

In `crates/tidemark/src/card.rs`, add the pure row plan above `impl Card`:

```rust
/// One drawable row of a card: a widget, resolved against its metric.
///
/// Pure and separate from the widgets so the interesting cases — a dangling metric
/// reference, a gauge with no denominator, an older daemon with no presentation at all —
/// are testable without a display.
pub(crate) struct Row<'a> {
    pub(crate) kind: WidgetKind,
    pub(crate) metric: &'a Metric,
    pub(crate) widget: &'a Widget,
}

/// The rows a status draws, in the published order.
///
/// A status with no presentation is an older daemon, not an empty reading: its windows are
/// mapped to gauges here so a rolling upgrade still draws cards. Remove this fallback once
/// the minimum daemon version publishes a presentation — see the spec's cutover note.
pub(crate) fn card_rows(status: &ProviderStatus) -> Vec<Row<'_>> {
    let Some(presentation) = status.presentation.as_ref() else {
        return Vec::new();
    };
    presentation
        .card
        .iter()
        .filter_map(|widget| {
            let kind = widget.kind()?;
            let metric = presentation.metric(&widget.metric)?;
            drawable(kind, metric, widget).then_some(Row { kind, metric, widget })
        })
        .collect()
}

/// Whether a widget has the numbers it needs. A gauge with no percentage and no positive
/// denominator has nothing honest to draw, so it is dropped rather than drawn at zero.
fn drawable(kind: WidgetKind, metric: &Metric, widget: &Widget) -> bool {
    match kind {
        WidgetKind::Gauge => gauge_percent(metric, widget).is_some(),
        WidgetKind::Value => match widget.field() {
            Some(Field::Text) | None => metric.text.is_some(),
            Some(field) => metric.field(field).is_some(),
        },
        WidgetKind::Ratio => match (widget.left(), widget.right()) {
            (Some(left), Some(right)) => {
                metric.field(left).is_some() && metric.field(right).is_some_and(|d| d > 0.0)
            }
            _ => false,
        },
        WidgetKind::Status => metric.text.is_some(),
    }
}

/// The percentage a gauge fills to: the reported one, or the quotient of its two fields.
pub(crate) fn gauge_percent(metric: &Metric, widget: &Widget) -> Option<f64> {
    if let Some(used) = metric.used_percent {
        return used.is_finite().then_some(used);
    }
    let numerator = metric.field(widget.left().unwrap_or(Field::Value))?;
    let denominator = metric.field(widget.right().unwrap_or(Field::Maximum))?;
    (denominator > 0.0).then(|| numerator / denominator * 100.0)
}
```

Then rewrite `Card::apply`'s body between the chip and `*self.shown.borrow_mut()`: build `card_rows(status)`; when it is non-empty draw the first row through the existing `self.bar`/`self.dominant_title`/`self.set_absolutes` widgets and the rest through `rebuild_rows`, which now takes `&[Row<'_>]` and builds a `QuotaBar` for a `Gauge` and a label for `Value`/`Ratio`/`Status` using `present::format_field`/`present::format_ratio`; when it is empty fall back to the pre-existing `windows`/`balance`/`blank` branches unchanged. Keep `plan`, `chip`, `balance_for` and `blank_message` exactly as they are — those are card chrome, not widgets.

For the pace mark, keep using the domain window: a `Row` whose metric has a `window` with `resets_at` and `length_secs` rebuilds a `Window` and asks `Window::pace`, as `retime` already does.

- [ ] **Step 8: Run the card tests**

Run: `cargo test -p tidemark card`
Expected: PASS.

- [ ] **Step 9: Render presentation sections in the detail dialog**

In `crates/tidemark/src/detail.rs`, change `rebuild_details` to prefer `status.presentation`'s `details`, mapping each item to the existing label/value row — the label is the metric's `title`, the value is `present::format_field` or `present::format_ratio` — and to fall back to `status.details` when there is no presentation. Add the test:

```rust
    #[test]
    fn detail_sections_come_from_the_presentation_in_its_own_order() {
        let status = presented_status();
        let rows = detail_rows(&status);
        assert_eq!(
            rows,
            [("Monthly cost".to_owned(), "12.5 of 50 USD".to_owned())],
            "the section order and the item order are the producer's"
        );
    }
```

Extract `detail_rows(&ProviderStatus) -> Vec<(String, String)>` as the pure function `rebuild_details` calls, so the test needs no dialog.

- [ ] **Step 10: Look at the real window**

Run the daemon and GUI and compare against the pre-change appearance for a gauge provider, a multi-window provider and a balance-only provider:

```bash
sudo systemctl --user stop tidemarkd 2>/dev/null || systemctl --user stop tidemarkd
cargo run -p tidemarkd &
xvfb-run -a --server-args='-screen 0 1280x900x24' sh -c \
  'cargo run -p tidemark & sleep 12; import -window root /tmp/claude-1000/cards.png'
```

Expected: the cards are visually unchanged — same leading bar, same secondary rows, same absolutes line. Open `/tmp/claude-1000/cards.png` and check by eye; a rearranged card means Task 2's order mapping is wrong, not the renderer.

- [ ] **Step 11: Run the gate and commit**

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
git add crates/tidemark crates/tidemark-types/src/present.rs crates/tidemark-types/src/lib.rs
git commit -m "feat(ui): render cards from the semantic presentation"
```

---

### Task 5: The `.tidemark-provider` schema

**Files:**
- Create: `crates/tidemark-core/src/plugin/mod.rs`
- Create: `crates/tidemark-core/src/plugin/limits.rs`
- Create: `crates/tidemark-core/src/plugin/schema.rs`
- Modify: `crates/tidemark-core/src/lib.rs` (`pub mod plugin;`)
- Test: colocated in `schema.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `plugin::limits::{PLUGIN_FILE_BYTES, LUA_SOURCE_BYTES, RESPONSE_BYTES, LUA_HEAP_BYTES, LUA_INSTRUCTIONS, MAX_METRICS, MAX_CARD_ITEMS, MAX_DETAIL_SECTIONS, MAX_SECTION_ITEMS, MAX_ID_BYTES, MAX_LABEL_BYTES, MAX_TEXT_BYTES, MAX_ERROR_BYTES}`
  - `plugin::PluginError` (one variant per stage)
  - `plugin::Definition { format_version: u32, id: String, name: String, plugin_version: String, method: Method, api_key_header: String, api_key_prefix: String, lua_source: String, icon_svg: Option<String>, bytes: Vec<u8> }`
  - `plugin::Method::{Get, Post}` with `as_str`
  - `plugin::schema::parse(bytes: &[u8], reserved: &[&str]) -> Result<Definition, PluginError>`
  - `plugin::schema::valid_id(id: &str) -> bool`

- [x] **Step 1: Write the failing tests**

Create `crates/tidemark-core/src/plugin/schema.rs` with the tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
format_version = 1

[provider]
id = "com.acme.quota"
name = "Acme AI"
plugin_version = "1.0.0"

[request]
method = "GET"
api_key_header = "Authorization"
api_key_prefix = "Bearer "

[parser]
language = "lua54"
source = '''
function parse(response, context)
    return { metrics = {}, card = {}, details = {} }
end
'''
"#;

    fn parse_str(text: &str) -> Result<Definition, PluginError> {
        parse(text.as_bytes(), &["zai", "claude"])
    }

    #[test]
    fn a_complete_definition_parses_into_its_declared_shape() {
        let definition = parse_str(MINIMAL).expect("the documented minimum parses");
        assert_eq!(definition.id, "com.acme.quota");
        assert_eq!(definition.name, "Acme AI");
        assert_eq!(definition.plugin_version, "1.0.0");
        assert_eq!(definition.method, Method::Get);
        assert_eq!(definition.api_key_header, "Authorization");
        assert_eq!(definition.api_key_prefix, "Bearer ");
        assert!(definition.icon_svg.is_none(), "an SVG is optional");
        assert!(definition.lua_source.contains("function parse"));
        assert_eq!(definition.bytes, MINIMAL.as_bytes(), "the exact validated bytes are kept");
    }

    #[test]
    fn an_unsupported_format_version_is_refused_before_anything_else_is_read() {
        let text = MINIMAL.replace("format_version = 1", "format_version = 2");
        assert!(matches!(
            parse_str(&text),
            Err(PluginError::UnsupportedFormat { found: 2, .. })
        ));
    }

    #[test]
    fn every_required_field_is_named_when_it_is_missing() {
        for (needle, expected) in [
            ("id = \"com.acme.quota\"\n", "provider.id"),
            ("name = \"Acme AI\"\n", "provider.name"),
            ("plugin_version = \"1.0.0\"\n", "provider.plugin_version"),
            ("method = \"GET\"\n", "request.method"),
            ("api_key_header = \"Authorization\"\n", "request.api_key_header"),
        ] {
            let text = MINIMAL.replace(needle, "");
            match parse_str(&text) {
                Err(PluginError::Schema { field, .. }) => assert_eq!(field, expected),
                other => panic!("{needle:?} removed should name {expected}: {other:?}"),
            }
        }
    }

    #[test]
    fn an_id_that_is_not_reverse_dns_is_refused() {
        for id in ["acme", "Com.Acme", "com..acme", ".com.acme", "com.acme.", "com acme", ""] {
            let text = MINIMAL.replace("com.acme.quota", id);
            assert!(parse_str(&text).is_err(), "id {id:?} must be refused");
        }
        assert!(valid_id("com.acme.quota"));
        assert!(valid_id("io.example.team-metrics"));
        assert!(!valid_id("acme"), "an id with no dot could collide with a built-in slug");
    }

    #[test]
    fn a_reserved_id_is_refused() {
        for id in ["zai", "claude", "tidemark.internal", "tidemark.zai"] {
            let text = MINIMAL.replace("com.acme.quota", id);
            assert!(
                matches!(parse_str(&text), Err(PluginError::ReservedId { .. })),
                "id {id:?} belongs to Tidemark"
            );
        }
    }

    #[test]
    fn only_get_and_empty_post_are_expressible() {
        let post = MINIMAL.replace("method = \"GET\"", "method = \"POST\"");
        assert_eq!(parse_str(&post).expect("POST parses").method, Method::Post);
        for method in ["PUT", "DELETE", "get", "PATCH"] {
            let text = MINIMAL.replace("\"GET\"", &format!("\"{method}\""));
            assert!(parse_str(&text).is_err(), "method {method} is outside the format");
        }
    }

    #[test]
    fn a_header_that_could_move_the_request_or_the_secret_is_refused() {
        for header in [
            "Host", "Cookie", "Connection", "Content-Length", "Transfer-Encoding",
            "Proxy-Authorization", "Proxy-Connection", "TE", "Trailer", "Upgrade",
            "host", "COOKIE",
        ] {
            let text = MINIMAL.replace("\"Authorization\"", &format!("\"{header}\""));
            assert!(
                matches!(parse_str(&text), Err(PluginError::ForbiddenHeader { .. })),
                "{header} must not carry a plugin's key"
            );
        }
    }

    #[test]
    fn a_header_name_that_is_not_a_header_name_is_refused() {
        for header in ["", "Auth orization", "Auth:orization", "Auth\nization", "Authörization"] {
            let text = MINIMAL.replace("\"Authorization\"", &format!("{header:?}"));
            assert!(parse_str(&text).is_err(), "{header:?} is not an RFC header name");
        }
    }

    #[test]
    fn a_missing_prefix_means_the_key_goes_in_bare() {
        let text = MINIMAL.replace("api_key_prefix = \"Bearer \"\n", "");
        assert_eq!(parse_str(&text).expect("the prefix is optional").api_key_prefix, "");
    }

    #[test]
    fn an_unknown_parser_language_is_refused_rather_than_assumed_to_be_lua() {
        let text = MINIMAL.replace("\"lua54\"", "\"lua53\"");
        assert!(parse_str(&text).is_err());
    }

    #[test]
    fn a_source_without_a_parse_function_is_refused_at_the_schema_stage() {
        let text = MINIMAL.replace("function parse(response, context)", "function transform(response)");
        assert!(matches!(parse_str(&text), Err(PluginError::Schema { .. })));
    }

    #[test]
    fn the_file_and_source_size_limits_are_enforced() {
        let long = "-".repeat(limits::LUA_SOURCE_BYTES + 1);
        let text = MINIMAL.replace("    return { metrics = {}, card = {}, details = {} }", &long);
        assert!(matches!(parse_str(&text), Err(PluginError::TooLarge { .. })));

        let huge = vec![b'x'; limits::PLUGIN_FILE_BYTES + 1];
        assert!(matches!(parse(&huge, &[]), Err(PluginError::TooLarge { .. })));
    }

    #[test]
    fn a_file_that_is_not_toml_or_not_utf8_is_a_schema_error_not_a_panic() {
        assert!(parse(b"\xff\xfe not utf8", &[]).is_err());
        assert!(parse(b"format_version = ", &[]).is_err());
    }

    #[test]
    fn a_name_longer_than_the_bound_is_refused() {
        let text = MINIMAL.replace("Acme AI", &"A".repeat(limits::MAX_LABEL_BYTES + 1));
        assert!(matches!(parse_str(&text), Err(PluginError::TooLarge { .. })));
    }
}
```

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemark-core plugin::schema`
Expected: FAIL — the module does not exist yet (add `pub mod plugin;` to `lib.rs` and `pub mod schema;` to `plugin/mod.rs` first so the failure is `cannot find function parse`).

- [x] **Step 3: Write the limits**

Create `crates/tidemark-core/src/plugin/limits.rs`:

```rust
//! Every bound the plugin runtime enforces, in one place and spelled out.
//!
//! They live together, and are documented verbatim in `docs/plugin-providers.md`, because a
//! limit a plugin author cannot look up is a limit they will hit by surprise. Exhausting one
//! is a provider failure — the account keeps its last good reading — never a daemon crash
//! and never a partially accepted reading.

/// The whole `.tidemark-provider` file, Lua and SVG included.
pub const PLUGIN_FILE_BYTES: usize = 512 * 1024;
/// The `[parser] source` block alone.
pub const LUA_SOURCE_BYTES: usize = 128 * 1024;
/// The HTTP response body, before JSON parsing.
pub const RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Lua heap attributable to one execution.
pub const LUA_HEAP_BYTES: usize = 16 * 1024 * 1024;
/// Lua VM instructions per execution.
pub const LUA_INSTRUCTIONS: u32 = 1_000_000;
/// Metrics in one reading.
pub const MAX_METRICS: usize = 128;
/// Widgets on the card.
pub const MAX_CARD_ITEMS: usize = 32;
/// Detail sections.
pub const MAX_DETAIL_SECTIONS: usize = 32;
/// Widgets in one detail section.
pub const MAX_SECTION_ITEMS: usize = 128;
/// A metric id.
pub const MAX_ID_BYTES: usize = 128;
/// A title, subtitle, section heading, provider name or unit.
pub const MAX_LABEL_BYTES: usize = 128;
/// A metric's status text.
pub const MAX_TEXT_BYTES: usize = 256;
/// A diagnostic excerpt taken from Lua or from a response.
pub const MAX_ERROR_BYTES: usize = 512;
```

- [x] **Step 4: Write the error type and the definition**

Create `crates/tidemark-core/src/plugin/mod.rs`:

```rust
//! User-installed providers: one file, treated as data.
//!
//! A `.tidemark-provider` file is TOML holding metadata, a request declaration, a pure Lua
//! transformation and an optional SVG mark. Nothing in it names a host, so its author
//! cannot choose where a recipient's key is sent; nothing in it can reach the filesystem,
//! the environment, the clock, the keyring or the network, because the sandbox has none of
//! those. Tidemark owns the single request and the header the key goes in, and the key never
//! enters Lua.
//!
//! The stages are separate on purpose, and every failure names exactly one of them: schema,
//! request declaration, SVG, Lua compile, HTTP, response size, JSON, Lua runtime, resource
//! limit, semantic output.

pub mod limits;
pub mod lua;
pub mod output;
pub mod provider;
pub mod schema;
pub mod svg;

use tidemark_types::{Presentation, Snapshot};

/// The one HTTP verb a plugin may declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The common case.
    Get,
    /// Always with an empty body: a plugin has no way to send one.
    Post,
}

impl Method {
    /// The declared spelling, which is also what the import preview shows the user.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// One validated plugin definition, ready to build accounts from.
#[derive(Debug, Clone)]
pub struct Definition {
    /// The file-format version this file declared. Always 1 for now.
    pub format_version: u32,
    /// Reverse-DNS provider id: the persistent storage key for accounts, keys and history.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The author's own SemVer string, shown at import and replacement. Informational.
    pub plugin_version: String,
    /// `GET`, or an empty-bodied `POST`.
    pub method: Method,
    /// The header Tidemark puts the account's key in.
    pub api_key_header: String,
    /// What goes in front of the key in that header. Often `Bearer `, often empty.
    pub api_key_prefix: String,
    /// The Lua chunk, as written.
    pub lua_source: String,
    /// The sanitized canonical SVG mark, when the file carried one and it survived
    /// sanitization. The original file keeps its own bytes; only this reaches memory and
    /// D-Bus. Filled by [`svg::sanitize`] — see Task 6.
    pub icon_svg: Option<String>,
    /// The exact validated file bytes, stored unchanged so the definition stays
    /// inspectable and exportable.
    pub bytes: Vec<u8>,
}

/// What one plugin reading produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The windows, for history, pace and notifications.
    pub snapshot: Snapshot,
    /// The layout, exactly as the plugin ordered it.
    pub presentation: Presentation,
}

/// A plugin failure, naming the one stage it happened in.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// A document could not be read at all: not UTF-8, not TOML, or not JSON. Used for the
    /// plugin file and for a response body, because "this is not the kind of document it claims
    /// to be" is one failure with two subjects.
    #[error("{path_hint} could not be read: {reason}")]
    Unreadable {
        /// What the subject is called in front of a person — `the plugin file`, `the response`.
        /// Never a whole filesystem path taken from a plugin.
        path_hint: String,
        /// The parser's own words.
        reason: String,
    },
    /// A file-format version this build does not implement.
    #[error("this build reads plugin format {supported}, and the file declares {found}")]
    UnsupportedFormat {
        /// What was declared.
        found: u32,
        /// What is implemented.
        supported: u32,
    },
    /// A required field is missing, or has the wrong type or an unusable value.
    #[error("{field}: {reason}")]
    Schema {
        /// The dotted field name, as the format documents it.
        field: &'static str,
        /// What was wrong with it.
        reason: String,
    },
    /// The id is Tidemark's rather than the author's.
    #[error("provider id {id} is reserved for Tidemark")]
    ReservedId {
        /// The refused id.
        id: String,
    },
    /// A header whose semantics can move the request, the connection or the secret.
    #[error("{header} cannot carry a plugin's key")]
    ForbiddenHeader {
        /// The refused header name.
        header: String,
    },
    /// Something exceeded one of [`limits`].
    #[error("{what} is {found} bytes, and the limit is {limit}")]
    TooLarge {
        /// Which bound was hit, in the words the guide uses.
        what: &'static str,
        /// What was measured.
        found: usize,
        /// The documented bound.
        limit: usize,
    },
    /// The SVG mark uses something outside the accepted static subset.
    #[error("the provider mark is not accepted: {reason}")]
    Svg {
        /// Which rule the document broke.
        reason: String,
    },
    /// The Lua chunk does not compile.
    #[error("the parser does not compile: {reason}")]
    LuaCompile {
        /// A bounded diagnostic with a plugin-relative line and column.
        reason: String,
    },
    /// The Lua chunk failed while running.
    #[error("the parser failed: {reason}")]
    LuaRuntime {
        /// A bounded diagnostic. Never a response body and never a credential.
        reason: String,
    },
    /// The Lua chunk exhausted a resource limit.
    #[error("the parser exceeded its {what} limit")]
    LuaExhausted {
        /// `instruction`, `memory`, or the output bound that was hit.
        what: &'static str,
    },
    /// The Lua return value is not a presentation this build can publish.
    #[error("the parser returned something unusable: {reason}")]
    Output {
        /// Which rule the return value broke.
        reason: String,
    },
}

/// The plugin file format this build implements.
pub const FORMAT_VERSION: u32 = 1;
```

- [x] **Step 5: Write the schema parser**

Above the tests in `crates/tidemark-core/src/plugin/schema.rs`:

```rust
//! Reading a `.tidemark-provider` file, and refusing the ones that are not one.
//!
//! Order matters: the size bound, then UTF-8, then TOML, then `format_version`, then
//! everything else. A file declaring a version this build does not implement is refused
//! before its Lua is looked at, so a future format cannot be half-read by an old build.

use super::{Definition, FORMAT_VERSION, Method, PluginError, limits};
use toml_edit::DocumentMut;

/// Header names a plugin may never name, lowercased.
///
/// Each of these changes where a request goes, how the connection is framed, or who else
/// sees the credential — which is exactly what the account owner, not the plugin author,
/// is supposed to decide.
const FORBIDDEN_HEADERS: &[&str] = &[
    "host",
    "cookie",
    "connection",
    "content-length",
    "transfer-encoding",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
];

/// Whether an id can be a plugin provider id: reverse-DNS, lowercase, with at least one dot.
///
/// The dot is the point. A plugin id without one could collide with a built-in slug the next
/// release adds, and a collision on a persistent storage key is a user's history filed under
/// somebody else's provider.
pub fn valid_id(id: &str) -> bool {
    if id.is_empty() || id.len() > limits::MAX_ID_BYTES || !id.contains('.') {
        return false;
    }
    let mut previous_separator = true;
    for byte in id.bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => previous_separator = false,
            b'.' | b'-' if !previous_separator => previous_separator = true,
            _ => return false,
        }
    }
    !previous_separator
}

/// Parses and validates a plugin file, given the provider ids this build already owns.
pub fn parse(bytes: &[u8], reserved: &[&str]) -> Result<Definition, PluginError> {
    bound("the plugin file", bytes.len(), limits::PLUGIN_FILE_BYTES)?;
    let text = std::str::from_utf8(bytes).map_err(|error| PluginError::Unreadable {
        path_hint: "the plugin file".to_owned(),
        reason: error.to_string(),
    })?;
    let document: DocumentMut = text.parse().map_err(|error: toml_edit::TomlError| {
        PluginError::Unreadable {
            path_hint: "the plugin file".to_owned(),
            reason: error.to_string(),
        }
    })?;

    let format_version = document
        .get("format_version")
        .and_then(|item| item.as_integer())
        .ok_or(PluginError::Schema {
            field: "format_version",
            reason: "must be an integer".to_owned(),
        })?;
    if format_version != i64::from(FORMAT_VERSION) {
        return Err(PluginError::UnsupportedFormat {
            found: format_version.try_into().unwrap_or(u32::MAX),
            supported: FORMAT_VERSION,
        });
    }

    let id = string(&document, "provider", "id", "provider.id")?;
    if !valid_id(&id) {
        return Err(PluginError::Schema {
            field: "provider.id",
            reason: "must be lowercase reverse-DNS with at least one dot, matching \
                     [a-z0-9]+(?:[.-][a-z0-9]+)*"
                .to_owned(),
        });
    }
    if id.starts_with("tidemark.") || id == "tidemark" || reserved.contains(&id.as_str()) {
        return Err(PluginError::ReservedId { id });
    }

    let name = string(&document, "provider", "name", "provider.name")?;
    bound("provider.name", name.len(), limits::MAX_LABEL_BYTES)?;
    let plugin_version = string(&document, "provider", "plugin_version", "provider.plugin_version")?;
    bound("provider.plugin_version", plugin_version.len(), limits::MAX_LABEL_BYTES)?;

    let method = match string(&document, "request", "method", "request.method")?.as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        other => {
            return Err(PluginError::Schema {
                field: "request.method",
                reason: format!("must be GET or POST, not {other:?}"),
            });
        }
    };

    let api_key_header = string(&document, "request", "api_key_header", "request.api_key_header")?;
    if !header_name(&api_key_header) {
        return Err(PluginError::Schema {
            field: "request.api_key_header",
            reason: "must be an RFC 9110 field name: visible ASCII without separators".to_owned(),
        });
    }
    if FORBIDDEN_HEADERS.contains(&api_key_header.to_ascii_lowercase().as_str()) {
        return Err(PluginError::ForbiddenHeader { header: api_key_header });
    }

    let api_key_prefix = optional_string(&document, "request", "api_key_prefix")?.unwrap_or_default();
    bound("request.api_key_prefix", api_key_prefix.len(), limits::MAX_LABEL_BYTES)?;
    if api_key_prefix.bytes().any(|byte| !(0x20..=0x7e).contains(&byte)) {
        return Err(PluginError::Schema {
            field: "request.api_key_prefix",
            reason: "must be printable ASCII".to_owned(),
        });
    }

    match string(&document, "parser", "language", "parser.language")?.as_str() {
        "lua54" => {}
        other => {
            return Err(PluginError::Schema {
                field: "parser.language",
                reason: format!("must be lua54, not {other:?}"),
            });
        }
    }
    let lua_source = string(&document, "parser", "source", "parser.source")?;
    bound("the parser source", lua_source.len(), limits::LUA_SOURCE_BYTES)?;
    if !lua_source.contains("function parse") {
        return Err(PluginError::Schema {
            field: "parser.source",
            reason: "must define a parse(response, context) function".to_owned(),
        });
    }

    let icon = optional_string(&document, "icon", "svg")?;

    Ok(Definition {
        format_version: FORMAT_VERSION,
        id,
        name,
        plugin_version,
        method,
        api_key_header,
        api_key_prefix,
        lua_source,
        // Sanitized in Task 6; the raw declaration never reaches the definition.
        icon_svg: icon.map(|_| String::new()).filter(|_| false),
        bytes: bytes.to_vec(),
    })
}

/// The raw `[icon] svg` declaration, for the sanitizer to accept or refuse.
pub fn declared_icon(bytes: &[u8]) -> Result<Option<String>, PluginError> {
    let text = std::str::from_utf8(bytes).map_err(|error| PluginError::Unreadable {
        path_hint: "the plugin file".to_owned(),
        reason: error.to_string(),
    })?;
    let document: DocumentMut = text.parse().map_err(|error: toml_edit::TomlError| {
        PluginError::Unreadable {
            path_hint: "the plugin file".to_owned(),
            reason: error.to_string(),
        }
    })?;
    optional_string(&document, "icon", "svg")
}

/// A required string field, named the way the format documents it.
fn string(
    document: &DocumentMut,
    table: &str,
    key: &str,
    field: &'static str,
) -> Result<String, PluginError> {
    document
        .get(table)
        .and_then(|item| item.get(key))
        .and_then(|item| item.as_str())
        .map(str::to_owned)
        .ok_or(PluginError::Schema {
            field,
            reason: "must be present and a string".to_owned(),
        })
}

/// An optional string field. Present-but-wrong is refused; absent is absent.
fn optional_string(
    document: &DocumentMut,
    table: &str,
    key: &str,
) -> Result<Option<String>, PluginError> {
    match document.get(table).and_then(|item| item.get(key)) {
        None => Ok(None),
        Some(item) => item.as_str().map(|text| Some(text.to_owned())).ok_or(PluginError::Schema {
            field: "an optional field",
            reason: format!("{table}.{key} is present and is not a string"),
        }),
    }
}

/// One size bound, refused with the numbers a plugin author needs to act on.
fn bound(what: &'static str, found: usize, limit: usize) -> Result<(), PluginError> {
    if found > limit {
        return Err(PluginError::TooLarge { what, found, limit });
    }
    Ok(())
}

/// Whether a string is an RFC 9110 field name.
fn header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'!' | b'#'..=b'\'' | b'*' | b'+' | b'-' | b'.' | b'^'..=b'`' | b'|' | b'~')
        })
}
```

Note the `icon_svg` line above is deliberately `None` until Task 6 wires the sanitizer in; `declared_icon` is what Task 6 consumes. Replace that expression with the sanitizer call in Task 6, Step 5.

- [x] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p tidemark-core plugin::schema`
Expected: PASS. `plugin/mod.rs` will not compile until Tasks 6–10 exist, so temporarily comment out the `pub mod lua; pub mod output; pub mod provider; pub mod svg;` lines and restore each as its task lands.

- [x] **Step 7: Commit**

```bash
git add crates/tidemark-core/src/plugin crates/tidemark-core/src/lib.rs
git commit -m "feat(core): parse and validate the plugin file format"
```

---

### Task 6: The SVG sanitizer

**Files:**
- Create: `crates/tidemark-core/src/plugin/svg.rs`
- Modify: `crates/tidemark-core/src/plugin/schema.rs` (call the sanitizer)
- Modify: `crates/tidemark-core/Cargo.toml` (`quick-xml`)
- Test: colocated

**Interfaces:**
- Consumes: `PluginError::Svg`, `limits`.
- Produces: `plugin::svg::sanitize(source: &str) -> Result<String, PluginError>` — canonical accepted SVG bytes.

- [x] **Step 1: Add the parser dependency**

```bash
cargo add --package tidemark-core quick-xml
```

Then annotate the new line in `crates/tidemark-core/Cargo.toml` in the style of its neighbours:

```toml
# quick-xml: the plugin SVG sanitizer. A pull parser and a writer, no DOM and no network —
# an XML library that resolved an external entity or a DTD would defeat the point of
# sanitizing at all.
```

- [x] **Step 2: Write the failing tests**

Create `crates/tidemark-core/src/plugin/svg.rs` with the tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const MARK: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <g transform="translate(2 2)"><path fill="currentColor" d="M0 0h10v10H0z"/></g>
  <circle cx="8" cy="8" r="4" fill="#336699"/>
</svg>"#;

    #[test]
    fn a_conservative_static_mark_is_accepted_and_serialized_canonically() {
        let clean = sanitize(MARK).expect("the documented subset is accepted");
        assert!(clean.starts_with("<svg"), "{clean}");
        assert!(clean.contains("viewBox=\"0 0 64 64\""));
        assert!(clean.contains("fill=\"currentColor\""));
        assert!(clean.contains("<circle"));
        assert_eq!(sanitize(&clean).expect("idempotent"), clean, "sanitizing twice changes nothing");
    }

    #[test]
    fn executable_and_interactive_content_is_refused() {
        for hostile in [
            r#"<svg><script>fetch("http://x")</script></svg>"#,
            r#"<svg><path onload="x()" d="M0 0"/></svg>"#,
            r#"<svg><animate attributeName="x" to="9"/></svg>"#,
            r#"<svg><foreignObject><div/></foreignObject></svg>"#,
            r#"<svg><filter id="f"/><path filter="url(#f)" d="M0 0"/></svg>"#,
            r#"<svg><style>path{fill:red}</style></svg>"#,
            r#"<svg><a href="http://x"><path d="M0 0"/></a></svg>"#,
        ] {
            assert!(matches!(sanitize(hostile), Err(PluginError::Svg { .. })), "{hostile}");
        }
    }

    #[test]
    fn every_reference_out_of_the_document_is_refused() {
        for hostile in [
            r#"<svg><image href="http://example.test/a.png"/></svg>"#,
            r#"<svg><image xlink:href="data:image/png;base64,AAA"/></svg>"#,
            r#"<svg><path fill="url(http://example.test/g)" d="M0 0"/></svg>"#,
            r#"<svg><use href="#other"/></svg>"#,
            r#"<?xml version="1.0"?><!DOCTYPE svg SYSTEM "http://example.test/svg.dtd"><svg/>"#,
            r#"<svg><!ENTITY x SYSTEM "file:///etc/passwd"></svg>"#,
        ] {
            assert!(matches!(sanitize(hostile), Err(PluginError::Svg { .. })), "{hostile}");
        }
    }

    #[test]
    fn a_document_that_is_not_an_svg_root_is_refused() {
        for hostile in [r#"<html><svg/></html>"#, "not xml at all", "", r#"<svg"#] {
            assert!(sanitize(hostile).is_err(), "{hostile:?}");
        }
    }

    #[test]
    fn an_oversized_mark_is_refused_before_it_is_parsed() {
        let huge = format!("<svg>{}</svg>", "<path d=\"M0 0\"/>".repeat(60_000));
        assert!(matches!(sanitize(&huge), Err(PluginError::TooLarge { .. })));
    }

    #[test]
    fn nesting_deep_enough_to_recurse_is_refused() {
        let deep = format!("<svg>{}{}</svg>", "<g>".repeat(200), "</g>".repeat(200));
        assert!(matches!(sanitize(&deep), Err(PluginError::Svg { .. })));
    }

    #[test]
    fn an_attribute_outside_the_accepted_set_is_dropped_rather_than_kept() {
        let clean = sanitize(r#"<svg><path d="M0 0" data-note="hi" tabindex="1"/></svg>"#)
            .expect("the geometry is fine");
        assert!(!clean.contains("data-note"));
        assert!(!clean.contains("tabindex"));
        assert!(clean.contains("d=\"M0 0\""));
    }
}
```

- [x] **Step 3: Run to verify they fail**

Run: `cargo test -p tidemark-core plugin::svg`
Expected: FAIL — `cannot find function sanitize`.

- [x] **Step 4: Write the sanitizer**

Above the tests:

```rust
//! The plugin's provider mark, reduced to a static picture.
//!
//! An SVG is a document format with scripting, external references and stylesheets in it,
//! and this one arrives from a file a stranger wrote. So it is not "checked": it is parsed,
//! matched against an allowlist of elements and attributes sufficient for a provider mark,
//! and *rewritten*. Anything the allowlist does not name is either refused — where keeping
//! it would change what the picture does — or dropped, where it is merely decoration.
//!
//! Refused rather than dropped, deliberately: a mark that carried a script is a file whose
//! author's intent we do not want to guess at, and telling them so is more useful than
//! silently showing them a different picture.

use super::{PluginError, limits};
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, Writer};

/// The largest mark accepted, well under the file bound so a mark alone cannot fill a file.
const MAX_SVG_BYTES: usize = 128 * 1024;
/// How deep grouping may nest. A provider mark needs a handful; a thousand is an attack.
const MAX_DEPTH: usize = 32;

/// Elements a provider mark may use.
const ELEMENTS: &[&str] = &[
    "svg", "g", "path", "rect", "circle", "ellipse", "line", "polyline", "polygon", "title",
];

/// Attributes accepted on any accepted element.
const ATTRIBUTES: &[&str] = &[
    "viewBox", "width", "height", "xmlns", "transform", "d", "x", "y", "x1", "y1", "x2", "y2",
    "cx", "cy", "r", "rx", "ry", "points", "fill", "fill-rule", "fill-opacity", "opacity",
];

/// Elements whose presence refuses the whole document.
const HOSTILE_ELEMENTS: &[&str] = &[
    "script", "style", "foreignobject", "image", "use", "a", "animate", "animatemotion",
    "animatetransform", "set", "filter", "clippath", "mask", "pattern", "lineargradient",
    "radialgradient", "symbol", "marker", "switch", "text", "textpath", "tspan", "iframe",
    "audio", "video", "handler", "listener",
];

/// Accepts a mark and returns canonical bytes, or says which rule it broke.
pub fn sanitize(source: &str) -> Result<String, PluginError> {
    if source.len() > MAX_SVG_BYTES {
        return Err(PluginError::TooLarge {
            what: "the provider mark",
            found: source.len(),
            limit: MAX_SVG_BYTES,
        });
    }
    let lowered = source.to_ascii_lowercase();
    for forbidden in ["<!doctype", "<!entity", "<?xml-stylesheet"] {
        if lowered.contains(forbidden) {
            return Err(svg(format!("{forbidden} is not allowed in a provider mark")));
        }
    }

    let mut reader = Reader::from_str(source);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = true;
    let mut writer = Writer::new(Vec::new());
    let mut depth = 0usize;
    let mut root_seen = false;

    loop {
        match reader.read_event().map_err(|error| svg(error.to_string()))? {
            Event::Eof => break,
            Event::Start(start) => {
                let name = local_name(start.name().as_ref())?;
                if !root_seen && name != "svg" {
                    return Err(svg("the document root must be <svg>".to_owned()));
                }
                root_seen = true;
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(svg(format!("nesting deeper than {MAX_DEPTH} elements")));
                }
                writer
                    .write_event(Event::Start(accept(&name, &start)?))
                    .map_err(|error| svg(error.to_string()))?;
            }
            Event::Empty(start) => {
                let name = local_name(start.name().as_ref())?;
                if !root_seen && name != "svg" {
                    return Err(svg("the document root must be <svg>".to_owned()));
                }
                root_seen = true;
                writer
                    .write_event(Event::Empty(accept(&name, &start)?))
                    .map_err(|error| svg(error.to_string()))?;
            }
            Event::End(end) => {
                let name = local_name(end.name().as_ref())?;
                depth = depth.saturating_sub(1);
                writer
                    .write_event(Event::End(quick_xml::events::BytesEnd::new(name)))
                    .map_err(|error| svg(error.to_string()))?;
            }
            // Text survives only inside <title>, which is the one place a mark has words.
            Event::Text(text) => {
                writer.write_event(Event::Text(text)).map_err(|error| svg(error.to_string()))?;
            }
            // Comments, processing instructions, CDATA and declarations are dropped: none of
            // them draws anything, and each is a place something executable has hidden.
            _ => {}
        }
    }

    if !root_seen {
        return Err(svg("no <svg> element was found".to_owned()));
    }
    String::from_utf8(writer.into_inner()).map_err(|error| svg(error.to_string()))
}

/// One element, with only the attributes the allowlist names and only local references.
fn accept(name: &str, start: &BytesStart<'_>) -> Result<BytesStart<'static>, PluginError> {
    if HOSTILE_ELEMENTS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(svg(format!("<{name}> is not allowed in a provider mark")));
    }
    if !ELEMENTS.contains(&name) {
        return Err(svg(format!("<{name}> is not part of the accepted subset")));
    }
    let mut element = BytesStart::new(name.to_owned());
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|error| svg(error.to_string()))?;
        let key = String::from_utf8_lossy(attribute.key.as_ref()).to_string();
        let lowered = key.to_ascii_lowercase();
        if lowered.starts_with("on") || lowered.contains("href") || lowered.starts_with("xlink:") {
            return Err(svg(format!("{key} is not allowed in a provider mark")));
        }
        if !ATTRIBUTES.contains(&key.as_str()) {
            continue;
        }
        let value = attribute.unescape_value().map_err(|error| svg(error.to_string()))?;
        if value.to_ascii_lowercase().contains("url(") || value.to_ascii_lowercase().contains("data:") {
            return Err(svg(format!("{key} references something outside the document")));
        }
        element.push_attribute(Attribute {
            key: attribute.key,
            value: value.as_bytes().to_vec().into(),
        });
    }
    Ok(element)
}

/// The element name without its namespace prefix.
fn local_name(raw: &[u8]) -> Result<String, PluginError> {
    let name = std::str::from_utf8(raw).map_err(|error| svg(error.to_string()))?;
    Ok(name.rsplit(':').next().unwrap_or(name).to_owned())
}

fn svg(reason: String) -> PluginError {
    let mut reason = reason;
    reason.truncate(limits::MAX_ERROR_BYTES);
    PluginError::Svg { reason }
}
```

- [x] **Step 5: Wire the sanitizer into the schema**

In `crates/tidemark-core/src/plugin/schema.rs`, replace the placeholder `icon_svg` expression with:

```rust
        icon_svg: match icon {
            Some(source) => Some(super::svg::sanitize(&source)?),
            None => None,
        },
```

and delete `declared_icon` if nothing else uses it. Add the schema test:

```rust
    #[test]
    fn a_mark_that_survives_sanitization_reaches_the_definition_canonically() {
        let text = format!(
            "{MINIMAL}\n[icon]\nsvg = '''\n<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 64 64\"><path fill=\"currentColor\" d=\"M0 0h1v1H0z\"/></svg>\n'''\n"
        );
        let icon = parse_str(&text).expect("parses").icon_svg.expect("a mark was declared");
        assert!(icon.contains("currentColor"));
    }

    #[test]
    fn a_hostile_mark_refuses_the_whole_file() {
        let text = format!("{MINIMAL}\n[icon]\nsvg = '''\n<svg><script>x()</script></svg>\n'''\n");
        assert!(matches!(parse_str(&text), Err(PluginError::Svg { .. })));
    }
```

- [x] **Step 6: Run the tests**

Run: `cargo test -p tidemark-core plugin::`
Expected: PASS.

- [x] **Step 7: Commit**

```bash
git add crates/tidemark-core/src/plugin crates/tidemark-core/Cargo.toml Cargo.lock
git commit -m "feat(core): sanitize a plugin's provider mark"
```

---

### Task 7: The Lua sandbox

**Files:**
- Create: `crates/tidemark-core/src/plugin/lua/mod.rs`
- Create: `crates/tidemark-core/src/plugin/lua/json.rs`
- Modify: `crates/tidemark-core/Cargo.toml` (`mlua`)
- Test: colocated in both

**Interfaces:**
- Consumes: `PluginError`, `limits`.
- Produces:
  - `plugin::lua::compile(source: &str) -> Result<(), PluginError>` — the import-time check.
  - `plugin::lua::run(source: &str, response: &serde_json::Value, captured_at: i64) -> Result<mlua::Value, PluginError>` — one execution, plus the `Lua` that owns the value (returned together as `Executed { lua: mlua::Lua, value: mlua::Value }`).
  - `plugin::lua::json::to_lua(&Lua, &serde_json::Value) -> mlua::Result<mlua::Value>`
  - `plugin::lua::json::NULL_SENTINEL: &str` — the global name the JSON null is bound to.
  - `plugin::lua::json::is_null(&Lua, &mlua::Value) -> bool`

- [x] **Step 1: Add the runtime and confirm its API**

```bash
cargo add --package tidemark-core mlua --features lua54,vendored
```

Annotate the line:

```toml
# mlua with vendored Lua 5.4: the plugin transformation language. Vendored so a user
# installs no interpreter and every platform runs the same VM; `lua54` for the goto/integer
# semantics the format documents. Measured at 0.45 MiB stripped against the alternatives —
# see the spec's runtime table, and the size gate in scripts.
```

Then confirm the four APIs this task depends on against the version Cargo resolved, because they are the ones that move between releases: `Lua::new_with(StdLib, LuaOptions)`, `Lua::set_memory_limit`, `Lua::set_hook`/`remove_hook` with `HookTriggers::every_nth_instruction`, and `Lua::sandbox`. Use the docs, not memory:

```bash
cargo doc -p mlua --no-deps --open 2>/dev/null || cargo tree -p tidemark-core -i mlua
```

If a signature differs from the code below, keep the behaviour and adapt the call — the tests in Step 2 are the contract, not the spelling.

- [x] **Step 2: Write the failing sandbox tests**

Create `crates/tidemark-core/src/plugin/lua/mod.rs` with the tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Runs a chunk whose `parse` returns whatever the fragment evaluates to, as a string,
    /// so a test can assert on the sandbox without going through output validation.
    fn evaluate(body: &str) -> Result<String, PluginError> {
        let source = format!("function parse(response, context)\n  return tostring({body})\nend");
        let executed = run(&source, &json!({}), 1_788_870_896)?;
        Ok(executed.value.as_str().unwrap_or_default().to_owned())
    }

    #[test]
    fn a_parse_function_receives_the_response_and_a_deterministic_context() {
        let source = "function parse(response, context)\n  \
                      return tostring(response.a.b) .. \"@\" .. tostring(context.captured_at)\nend";
        let executed = run(&source, &json!({"a": {"b": 7}}), 1_788_870_896).expect("runs");
        assert_eq!(executed.value.as_str().unwrap(), "7@1788870896");
    }

    #[test]
    fn the_dangerous_standard_libraries_are_not_merely_hidden_but_absent() {
        for global in [
            "io", "os", "package", "debug", "require", "dofile", "load", "loadfile",
            "collectgarbage", "pairs", "next", "pcall", "xpcall", "getmetatable",
            "setmetatable", "rawset", "rawget", "coroutine", "arg", "print",
        ] {
            assert_eq!(
                evaluate(global).expect("the chunk itself is fine").as_str(),
                "nil",
                "{global} must not exist in the sandbox"
            );
        }
        assert_eq!(evaluate("string.dump").unwrap(), "nil");
        assert_eq!(evaluate("math.random").unwrap(), "nil");
        assert_eq!(evaluate("math.randomseed").unwrap(), "nil");
    }

    #[test]
    fn the_documented_standard_library_subset_is_present() {
        for global in ["assert", "error", "ipairs", "select", "tostring", "type", "math", "string", "table"] {
            assert_ne!(evaluate(global).unwrap().as_str(), "nil", "{global} is documented as available");
        }
        assert_eq!(evaluate("math.floor(3.7)").unwrap(), "3");
        assert_eq!(evaluate("string.upper(\"ab\")").unwrap(), "AB");
    }

    #[test]
    fn there_is_no_way_to_read_the_clock() {
        assert_eq!(evaluate("now").unwrap(), "nil");
        assert_eq!(evaluate("os").unwrap(), "nil", "os.time is the clock, and os is gone");
    }

    #[test]
    fn an_infinite_loop_is_stopped_at_the_instruction_limit() {
        let source = "function parse(response, context)\n  while true do end\nend";
        assert!(matches!(
            run(source, &json!({}), 0),
            Err(PluginError::LuaExhausted { what: "instruction" })
        ));
    }

    #[test]
    fn unbounded_recursion_is_stopped_rather_than_overflowing_the_host_stack() {
        let source = "local function f(n) return f(n + 1) end\n\
                      function parse(response, context) return f(1) end";
        assert!(matches!(
            run(source, &json!({}), 0),
            Err(PluginError::LuaExhausted { .. }) | Err(PluginError::LuaRuntime { .. })
        ));
    }

    #[test]
    fn unbounded_allocation_is_stopped_at_the_memory_limit() {
        let source = "function parse(response, context)\n  \
                      local t = {}\n  local i = 1\n  \
                      while true do t[i] = string.rep(\"x\", 4096) i = i + 1 end\nend";
        assert!(matches!(
            run(source, &json!({}), 0),
            Err(PluginError::LuaExhausted { .. })
        ));
    }

    #[test]
    fn a_chunk_that_does_not_compile_says_where() {
        let error = compile("function parse( then end").expect_err("this is not Lua");
        let PluginError::LuaCompile { reason } = error else { panic!("wrong stage: {error:?}") };
        assert!(reason.contains('1'), "a compile diagnostic carries a line: {reason}");
        assert!(reason.len() <= limits::MAX_ERROR_BYTES);
    }

    #[test]
    fn a_chunk_with_no_parse_function_fails_at_run_time_with_its_own_message() {
        let error = run("local x = 1", &json!({}), 0).expect_err("there is nothing to call");
        assert!(matches!(error, PluginError::LuaRuntime { .. }));
    }

    #[test]
    fn top_level_evaluation_runs_under_the_same_sandbox() {
        let source = "local escaped = os\nfunction parse(response, context) return tostring(escaped) end";
        assert_eq!(
            run(source, &json!({}), 0).expect("runs").value.as_str().unwrap(),
            "nil",
            "the chunk's top level cannot see what parse cannot"
        );
    }

    #[test]
    fn an_error_raised_by_the_plugin_reaches_the_caller_bounded() {
        let long = "e".repeat(limits::MAX_ERROR_BYTES * 4);
        let source = format!("function parse(response, context) error(\"{long}\") end");
        let PluginError::LuaRuntime { reason } = run(&source, &json!({}), 0).expect_err("raises")
        else {
            panic!("a plugin error is a runtime failure")
        };
        assert!(reason.len() <= limits::MAX_ERROR_BYTES);
    }

    #[test]
    fn two_executions_share_no_state() {
        let source = "counter = (counter or 0) + 1\n\
                      function parse(response, context) return tostring(counter) end";
        assert_eq!(run(source, &json!({}), 0).unwrap().value.as_str().unwrap(), "1");
        assert_eq!(
            run(source, &json!({}), 0).unwrap().value.as_str().unwrap(),
            "1",
            "a fresh environment per execution, so one account cannot see another's"
        );
    }
}
```

And `crates/tidemark-core/src/plugin/lua/json.rs`'s tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn convert(value: serde_json::Value, expression: &str) -> String {
        let lua = mlua::Lua::new();
        let converted = to_lua(&lua, &value).expect("converts");
        lua.globals().set("response", converted).expect("bound");
        lua.load(format!("return tostring({expression})")).eval().expect("evaluates")
    }

    #[test]
    fn objects_become_tables_and_arrays_become_one_based_sequences() {
        assert_eq!(convert(json!({"a": {"b": [10, 20]}}), "response.a.b[1]"), "10");
        assert_eq!(convert(json!({"a": {"b": [10, 20]}}), "#response.a.b"), "2");
    }

    #[test]
    fn scalars_keep_their_json_meaning() {
        assert_eq!(convert(json!({"n": 1.5}), "response.n"), "1.5");
        assert_eq!(convert(json!({"n": 3}), "response.n"), "3");
        assert_eq!(convert(json!({"s": "12"}), "response.s"), "12");
        assert_eq!(convert(json!({"b": true}), "response.b"), "true");
    }

    #[test]
    fn json_null_is_a_sentinel_and_not_a_missing_key() {
        let lua = mlua::Lua::new();
        install_null(&lua).expect("installs");
        let value = to_lua(&lua, &json!({"present": null})).expect("converts");
        lua.globals().set("response", value).expect("bound");
        let answer: String = lua
            .load("return tostring(response.present ~= nil) .. tostring(response.absent == nil)")
            .eval()
            .expect("evaluates");
        assert_eq!(answer, "truetrue", "null is present; a missing key is not");
    }

    #[test]
    fn a_non_finite_number_cannot_enter_the_sandbox() {
        // serde_json cannot hold one, so this guards the boundary the other way: a value
        // the parser produced must be rejected downstream, never silently become nil here.
        assert!(serde_json::from_str::<serde_json::Value>("NaN").is_err());
    }
}
```

- [x] **Step 3: Run to verify they fail**

Run: `cargo test -p tidemark-core plugin::lua`
Expected: FAIL — `cannot find function run`.

- [x] **Step 4: Write the JSON bridge**

Above the tests in `crates/tidemark-core/src/plugin/lua/json.rs`:

```rust
//! JSON as the sandbox sees it.
//!
//! The mapping is the format's contract and is documented for plugin authors: objects are
//! tables with string keys, arrays are one-based sequences, and scalars keep their meaning.
//! `null` is the one case with a decision in it — Lua's `nil` cannot be told apart from a
//! missing key, and "the provider said the limit is null" and "the provider did not mention
//! a limit" are different facts a plugin must be able to branch on. So null becomes a
//! read-only sentinel table bound to the global `null`.

use mlua::{Lua, Table, Value};
use serde_json::Value as Json;

/// The global a JSON null arrives as.
pub const NULL_SENTINEL: &str = "null";

/// Creates the sentinel and binds it. Called once per execution.
pub fn install_null(lua: &Lua) -> mlua::Result<()> {
    let sentinel = lua.create_table()?;
    // Read-only: a plugin that could write into the sentinel could make one poll's null
    // look like another's, and the sentinel is shared by every value in the document.
    sentinel.set_readonly(true);
    lua.globals().set(NULL_SENTINEL, sentinel)?;
    lua.globals().set_readonly(false)?;
    Ok(())
}

/// The sentinel, for comparisons.
fn sentinel(lua: &Lua) -> mlua::Result<Value> {
    lua.globals().get(NULL_SENTINEL)
}

/// Whether a value is the JSON null sentinel.
pub fn is_null(lua: &Lua, value: &Value) -> bool {
    match (sentinel(lua), value) {
        (Ok(Value::Table(expected)), Value::Table(found)) => expected == *found,
        _ => false,
    }
}

/// One JSON document as Lua values.
pub fn to_lua(lua: &Lua, value: &Json) -> mlua::Result<Value> {
    Ok(match value {
        Json::Null => sentinel(lua).unwrap_or(Value::Nil),
        Json::Bool(flag) => Value::Boolean(*flag),
        Json::Number(number) => match number.as_i64() {
            Some(integer) => Value::Integer(integer),
            None => Value::Number(number.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(text) => Value::String(lua.create_string(text)?),
        Json::Array(items) => {
            let table: Table = lua.create_table_with_capacity(items.len(), 0)?;
            for (index, item) in items.iter().enumerate() {
                table.set(index + 1, to_lua(lua, item)?)?;
            }
            Value::Table(table)
        }
        Json::Object(fields) => {
            let table: Table = lua.create_table_with_capacity(0, fields.len())?;
            for (key, field) in fields {
                table.set(key.as_str(), to_lua(lua, field)?)?;
            }
            Value::Table(table)
        }
    })
}
```

- [x] **Step 5: Write the sandbox**

Above the tests in `crates/tidemark-core/src/plugin/lua/mod.rs`:

```rust
//! The environment a plugin's `parse` runs in, and the limits it runs under.
//!
//! Built rather than restricted: the VM starts with no standard library at all
//! (`StdLib::NONE`) and the documented subset is put back one name at a time. That way a new
//! `mlua` release cannot quietly reintroduce a library, and the list of what a plugin can
//! reach is a list in this file rather than a claim about a default.
//!
//! Two removals need saying out loud. **`pairs` and `next` are gone**, so a plugin cannot
//! iterate a JSON object in hash order — the result would differ between platforms, and the
//! output order is part of what a plugin publishes. `sorted_keys` is the documented
//! replacement. **`pcall` is gone**, so a plugin cannot swallow the error that a limit was
//! hit; a limit is the daemon's verdict, not a condition to recover from.
//!
//! Every execution gets its own `Lua`. Nothing is shared between accounts or polls.

pub mod api;
pub mod json;

use super::{PluginError, limits};
use mlua::{HookTriggers, Lua, LuaOptions, StdLib, Value, VmState};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// One finished execution, holding the VM its value belongs to.
///
/// The `Lua` travels with the value because an `mlua::Value` borrows its state: dropping the
/// VM to return only the value would be a use-after-free the type system already refuses.
pub struct Executed {
    /// The VM the value lives in.
    pub lua: Lua,
    /// What `parse` returned.
    pub value: Value,
}

/// Compiles a chunk without running it: the check import performs before storing a file.
pub fn compile(source: &str) -> Result<(), PluginError> {
    let lua = sandbox(&Arc::new(AtomicBool::new(false)))
        .map_err(|error| PluginError::LuaCompile { reason: bounded(&error.to_string()) })?;
    lua.load(source)
        .set_name("plugin")
        .into_function()
        .map(|_| ())
        .map_err(|error| PluginError::LuaCompile { reason: bounded(&error.to_string()) })
}

/// Runs `parse(response, context)` once, under every limit.
pub fn run(source: &str, response: &serde_json::Value, captured_at: i64) -> Result<Executed, PluginError> {
    let exhausted = Arc::new(AtomicBool::new(false));
    let lua = sandbox(&exhausted).map_err(|error| runtime(&error, &exhausted))?;

    let value = (|| -> mlua::Result<Value> {
        json::install_null(&lua)?;
        api::install(&lua)?;
        lua.load(source).set_name("plugin").exec()?;

        let context = lua.create_table()?;
        context.set("captured_at", captured_at)?;
        let response = json::to_lua(&lua, response)?;

        let parse: mlua::Function = lua.globals().get("parse")?;
        parse.call((response, context))
    })();

    match value {
        Ok(value) => Ok(Executed { lua, value }),
        Err(error) => Err(runtime(&error, &exhausted)),
    }
}

/// A VM with nothing in it but the documented subset.
fn sandbox(exhausted: &Arc<AtomicBool>) -> mlua::Result<Lua> {
    let lua = Lua::new_with(
        StdLib::MATH | StdLib::STRING | StdLib::TABLE,
        LuaOptions::default(),
    )?;
    lua.set_memory_limit(limits::LUA_HEAP_BYTES)?;

    let globals = lua.globals();
    // Everything the base library brings that the format does not document. Removed by name
    // rather than trusted to be absent: `StdLib::NONE` still leaves the base library's own
    // globals in place, and a future release could add one.
    for name in [
        "collectgarbage", "dofile", "load", "loadfile", "loadstring", "require", "next",
        "pairs", "pcall", "xpcall", "rawequal", "rawget", "rawset", "rawlen", "getmetatable",
        "setmetatable", "print", "unpack", "coroutine", "io", "os", "package", "debug",
        "utf8", "arg", "_G",
    ] {
        globals.set(name, Value::Nil)?;
    }
    // The two libraries that are kept still carry functions that load a chunk or reach for
    // entropy. A plugin is a pure function of its input; neither belongs in one.
    if let Ok(string) = globals.get::<mlua::Table>("string") {
        string.set("dump", Value::Nil)?;
    }
    if let Ok(math) = globals.get::<mlua::Table>("math") {
        math.set("random", Value::Nil)?;
        math.set("randomseed", Value::Nil)?;
    }

    let flag = Arc::clone(exhausted);
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(limits::LUA_INSTRUCTIONS),
        move |_lua, _debug| {
            flag.store(true, Ordering::Relaxed);
            Err(mlua::Error::runtime("instruction limit reached"))
        },
    );
    Ok(lua)
}

/// Classifies a failure: a limit that was hit, or the plugin's own error.
fn runtime(error: &mlua::Error, exhausted: &Arc<AtomicBool>) -> PluginError {
    if exhausted.load(Ordering::Relaxed) {
        return PluginError::LuaExhausted { what: "instruction" };
    }
    let text = error.to_string();
    if text.contains("not enough memory") || matches!(error, mlua::Error::MemoryError(_)) {
        return PluginError::LuaExhausted { what: "memory" };
    }
    if text.contains("stack overflow") {
        return PluginError::LuaExhausted { what: "stack" };
    }
    PluginError::LuaRuntime { reason: bounded(&text) }
}

/// A diagnostic cut to the documented bound, on a character boundary.
///
/// Bounded rather than whole: a plugin's `error()` message is data an endpoint may have
/// influenced, and an unbounded one would carry a response body into a log.
fn bounded(reason: &str) -> String {
    let mut end = reason.len().min(limits::MAX_ERROR_BYTES);
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}
```

- [x] **Step 6: Run the tests**

Run: `cargo test -p tidemark-core plugin::lua`
Expected: PASS. If `unbounded_recursion...` reports `LuaRuntime` rather than `LuaExhausted`, the assertion already accepts both — Lua's own stack limit catching it first is a correct outcome.

- [x] **Step 7: Commit**

```bash
git add crates/tidemark-core/src/plugin/lua crates/tidemark-core/Cargo.toml Cargo.lock
git commit -m "feat(core): sandboxed Lua 5.4 runtime for plugin parsers"
```

---

### Task 8: The host API

**Files:**
- Create: `crates/tidemark-core/src/plugin/lua/api.rs`
- Test: colocated

**Interfaces:**
- Consumes: Task 7's sandbox, `json::is_null`, `json::NULL_SENTINEL`.
- Produces: `plugin::lua::api::install(&Lua) -> mlua::Result<()>`, binding `number`, `percent`, `parse_time`, `is_null`, `sorted_keys`, `gauge`, `value`, `ratio`, `status`. Marker tables carry `__widget` (the kind string) plus `metric` and the option keys.

- [x] **Step 1: Write the failing tests**

Create `crates/tidemark-core/src/plugin/lua/api.rs` with the tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expression: &str) -> Result<String, String> {
        let lua = mlua::Lua::new();
        super::super::json::install_null(&lua).expect("null");
        install(&lua).expect("api");
        lua.load(format!("return tostring({expression})"))
            .eval()
            .map_err(|error| error.to_string())
    }

    #[test]
    fn number_accepts_a_finite_number_or_a_numeric_string() {
        assert_eq!(eval("number(12.5)").unwrap(), "12.5");
        assert_eq!(eval("number(\"12.5\")").unwrap(), "12.5");
        assert_eq!(eval("number(\" 42 \")").unwrap(), "42");
    }

    #[test]
    fn number_refuses_anything_that_is_not_one() {
        for hostile in ["number(\"a lot\")", "number(nil)", "number(null)", "number({})", "number(true)"] {
            assert!(eval(hostile).is_err(), "{hostile} must be a parse error");
        }
    }

    #[test]
    fn percent_divides_and_refuses_a_non_positive_maximum() {
        assert_eq!(eval("percent(1, 4)").unwrap(), "25.0");
        assert!(eval("percent(1, 0)").is_err(), "a zero maximum has no percentage");
        assert!(eval("percent(1, -4)").is_err());
    }

    #[test]
    fn percent_above_one_hundred_is_truthful_rather_than_clamped() {
        assert_eq!(eval("percent(5, 4)").unwrap(), "125.0");
    }

    #[test]
    fn parse_time_reads_rfc_3339_and_unix_seconds() {
        assert_eq!(eval("parse_time(\"2026-09-08T12:00:00Z\")").unwrap(), "1788782400");
        assert_eq!(eval("parse_time(1788782400)").unwrap(), "1788782400");
        assert_eq!(eval("parse_time(\"1788782400\")").unwrap(), "1788782400");
        assert!(eval("parse_time(\"yesterday\")").is_err());
    }

    #[test]
    fn is_null_is_true_only_for_the_sentinel() {
        assert_eq!(eval("is_null(null)").unwrap(), "true");
        assert_eq!(eval("is_null(nil)").unwrap(), "false");
        assert_eq!(eval("is_null(0)").unwrap(), "false");
        assert_eq!(eval("is_null({})").unwrap(), "false");
    }

    #[test]
    fn sorted_keys_replaces_the_unordered_iteration_that_was_removed() {
        let lua = mlua::Lua::new();
        super::super::json::install_null(&lua).expect("null");
        install(&lua).expect("api");
        let joined: String = lua
            .load(
                "local keys = sorted_keys({ b = 1, a = 2, c = 3 })\n\
                 local out = \"\"\n\
                 for _, key in ipairs(keys) do out = out .. key end\n\
                 return out",
            )
            .eval()
            .expect("evaluates");
        assert_eq!(joined, "abc", "lexical order, so two platforms agree");
    }

    #[test]
    fn sorted_keys_refuses_something_that_is_not_an_object() {
        assert!(eval("sorted_keys(4)").is_err());
    }

    #[test]
    fn a_widget_helper_returns_a_typed_marker_carrying_its_options() {
        let lua = mlua::Lua::new();
        super::super::json::install_null(&lua).expect("null");
        install(&lua).expect("api");
        let table: mlua::Table = lua
            .load("return gauge(\"cost\", { field = \"used_percent\", emphasis = \"compact\" })")
            .eval()
            .expect("evaluates");
        assert_eq!(table.get::<String>(MARKER).unwrap(), "gauge");
        assert_eq!(table.get::<String>("metric").unwrap(), "cost");
        assert_eq!(table.get::<String>("field").unwrap(), "used_percent");
        assert_eq!(table.get::<String>("emphasis").unwrap(), "compact");
    }

    #[test]
    fn every_widget_helper_names_its_own_kind_and_takes_no_options_it_does_not_have() {
        for (call, kind) in [
            ("gauge(\"m\", {})", "gauge"),
            ("value(\"m\", {})", "value"),
            ("ratio(\"m\", { left = \"value\", right = \"maximum\" })", "ratio"),
            ("status(\"m\", {})", "status"),
        ] {
            assert_eq!(eval(&format!("({call})[\"{MARKER}\"]")).unwrap(), kind);
        }
        assert!(eval("gauge(nil, {})").is_err(), "a widget must name a metric");
        assert!(eval("gauge(\"m\", { colour = \"red\" })").is_err(), "there is no colour to choose");
        assert!(eval("gauge(\"m\", { field = 4 })").is_err());
    }

    #[test]
    fn a_widget_helper_takes_its_options_table_optionally() {
        assert_eq!(eval(&format!("value(\"m\")[\"{MARKER}\"]")).unwrap(), "value");
    }
}
```

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemark-core plugin::lua::api`
Expected: FAIL — `cannot find function install`.

- [x] **Step 3: Write the host API**

```rust
//! The small pure API a plugin gets, and nothing else.
//!
//! Every function here is a pure function of its arguments. None of them reaches state, the
//! clock, the filesystem or the network, and none of them can be given a colour, a pixel or
//! a format string: the four widget helpers return *marker tables* naming a semantic widget,
//! and the spelling of the numbers stays Tidemark's, so a plugin card and a built-in card
//! read the same way. See `docs/plugin-providers.md`.

use super::json;
use mlua::{Lua, Table, Value, Variadic};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The key a widget marker names its kind under. Not a Lua-visible feature: it is how
/// `output.rs` recognises what a plugin returned.
pub const MARKER: &str = "__widget";

/// Option keys each widget accepts. Anything else is a mistake worth naming, because a
/// plugin author who typed `colour` expects it to have done something.
const GAUGE_OPTIONS: &[&str] = &["field", "left", "right", "format", "emphasis"];
const VALUE_OPTIONS: &[&str] = &["field", "format", "emphasis"];
const RATIO_OPTIONS: &[&str] = &["left", "right", "format", "emphasis"];
const STATUS_OPTIONS: &[&str] = &["field", "emphasis"];

/// Binds every host global. Called once per execution, before the chunk runs.
pub fn install(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();

    globals.set("number", lua.create_function(|_, value: Value| coerce(&value))?)?;

    globals.set(
        "percent",
        lua.create_function(|_, (value, maximum): (Value, Value)| {
            let (value, maximum) = (coerce(&value)?, coerce(&maximum)?);
            if !(maximum > 0.0) {
                return Err(mlua::Error::runtime(
                    "percent needs a maximum greater than zero; a provider that reported none has no percentage",
                ));
            }
            Ok(value / maximum * 100.0)
        })?,
    )?;

    globals.set(
        "parse_time",
        lua.create_function(|_, value: Value| match &value {
            Value::Integer(seconds) => Ok(*seconds),
            Value::Number(seconds) if seconds.is_finite() => Ok(seconds.round() as i64),
            Value::String(text) => {
                let text = text.to_str()?;
                let trimmed = text.trim();
                if let Ok(seconds) = trimmed.parse::<i64>() {
                    return Ok(seconds);
                }
                OffsetDateTime::parse(trimmed, &Rfc3339)
                    .map(OffsetDateTime::unix_timestamp)
                    .map_err(|_| {
                        mlua::Error::runtime(
                            "parse_time takes RFC 3339 or Unix seconds".to_owned(),
                        )
                    })
            }
            other => Err(mlua::Error::runtime(format!(
                "parse_time cannot read a {}",
                other.type_name()
            ))),
        })?,
    )?;

    globals.set(
        "is_null",
        lua.create_function(|lua, value: Value| Ok(json::is_null(lua, &value)))?,
    )?;

    globals.set(
        "sorted_keys",
        lua.create_function(|lua, table: Value| {
            let Value::Table(table) = table else {
                return Err(mlua::Error::runtime("sorted_keys takes a JSON object"));
            };
            let mut keys: Vec<String> = Vec::new();
            // `pairs` is not available to a plugin, but the host may iterate: what is
            // withheld is *unordered* iteration, and this is where the order is imposed.
            for entry in table.pairs::<Value, Value>() {
                let (key, _) = entry?;
                if let Value::String(key) = key {
                    keys.push(key.to_str()?.to_owned());
                }
            }
            keys.sort_unstable();
            let out = lua.create_table_with_capacity(keys.len(), 0)?;
            for (index, key) in keys.into_iter().enumerate() {
                out.set(index + 1, key)?;
            }
            Ok(out)
        })?,
    )?;

    widget(lua, "gauge", GAUGE_OPTIONS)?;
    widget(lua, "value", VALUE_OPTIONS)?;
    widget(lua, "ratio", RATIO_OPTIONS)?;
    widget(lua, "status", STATUS_OPTIONS)?;
    Ok(())
}

/// One widget helper: `kind(metric_id, options?)`.
fn widget(lua: &Lua, kind: &'static str, accepted: &'static [&'static str]) -> mlua::Result<()> {
    let function = lua.create_function(move |lua, mut args: Variadic<Value>| {
        let metric = match args.first() {
            Some(Value::String(id)) => id.to_str()?.to_owned(),
            _ => {
                return Err(mlua::Error::runtime(format!(
                    "{kind} needs a metric id as its first argument"
                )));
            }
        };
        let marker: Table = lua.create_table()?;
        marker.set(MARKER, kind)?;
        marker.set("metric", metric)?;

        if args.len() > 1 {
            let options = args.remove(1);
            let Value::Table(options) = options else {
                return Err(mlua::Error::runtime(format!("{kind} options must be a table")));
            };
            for entry in options.pairs::<Value, Value>() {
                let (key, value) = entry?;
                let Value::String(key) = key else {
                    return Err(mlua::Error::runtime(format!("{kind} options are named")));
                };
                let key = key.to_str()?.to_owned();
                if !accepted.contains(&key.as_str()) {
                    return Err(mlua::Error::runtime(format!(
                        "{kind} has no {key} option; it may set {}",
                        accepted.join(", ")
                    )));
                }
                let Value::String(value) = value else {
                    return Err(mlua::Error::runtime(format!(
                        "{kind} option {key} must be a string"
                    )));
                };
                marker.set(key, value.to_str()?.to_owned())?;
            }
        }
        Ok(marker)
    })?;
    lua.globals().set(kind, function)
}

/// A finite number, or a string that is one. Nothing else, and never a default.
fn coerce(value: &Value) -> mlua::Result<f64> {
    let number = match value {
        Value::Integer(integer) => *integer as f64,
        Value::Number(number) => *number,
        Value::String(text) => text
            .to_str()?
            .trim()
            .parse::<f64>()
            .map_err(|_| mlua::Error::runtime("number takes a number or a numeric string"))?,
        other => {
            return Err(mlua::Error::runtime(format!(
                "number cannot read a {}",
                other.type_name()
            )));
        }
    };
    if !number.is_finite() {
        return Err(mlua::Error::runtime("a quota number must be finite"));
    }
    Ok(number)
}
```

- [x] **Step 4: Run the tests**

Run: `cargo test -p tidemark-core plugin::lua::api`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/tidemark-core/src/plugin/lua/api.rs
git commit -m "feat(core): the pure host API a plugin parser calls"
```

---

### Task 9: Typed output validation

**Files:**
- Create: `crates/tidemark-core/src/plugin/output.rs`
- Test: colocated

**Interfaces:**
- Consumes: Task 7's `Executed`, Task 8's `MARKER`, Task 1's `Metric`/`Widget`/`Presentation`, `limits`, `PluginError::Output`.
- Produces: `plugin::output::validate(executed: &Executed, provider: &ProviderId, account: &AccountId, captured_at: Timestamp) -> Result<Reading, PluginError>`.

- [x] **Step 1: Write the failing tests**

Create `crates/tidemark-core/src/plugin/output.rs` with the tests first:

```rust
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
        assert_eq!(reading.presentation.metrics.len(), 1, "one metric, three widgets");
        let kinds: Vec<&str> = reading.presentation.card.iter().map(|w| w.kind.as_str()).collect();
        assert_eq!(kinds, ["gauge", "value", "ratio"], "and the order is the plugin's");
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
        assert_eq!(windows[0].resets_at.map(|t| t.as_unix()), Some(1_788_874_496));
        assert_eq!(windows[1].resets_at, None, "no reset was declared, so none is published");
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
            ("a dangling card reference", r#"return { metrics = {}, card = { gauge("ghost", {}) }, details = {} }"#),
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
            ("a result with no metrics field", "return { card = {}, details = {} }"),
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
        assert!(matches!(error, Err(PluginError::Output { .. })), "{error:?}");
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
        assert!(matches!(reading(&long_text), Err(PluginError::Output { .. })));
    }

    #[test]
    fn the_snapshot_is_filed_under_the_account_being_polled() {
        let reading = reading(ONE_METRIC).expect("validates");
        assert_eq!(reading.snapshot.provider.as_str(), "com.acme.quota");
        assert_eq!(reading.snapshot.account.as_str(), "default");
        assert_eq!(reading.snapshot.captured_at.as_unix(), AT);
    }
}
```

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemark-core plugin::output`
Expected: FAIL — `cannot find function validate`.

- [x] **Step 3: Write the validator**

```rust
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
    AccountId, Field, Metric, MetricWindow, Presentation, PresentedSection, ProviderId, Snapshot,
    Timestamp, Widget, WidgetKind, Window, WindowKey, WindowLength,
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
        return Err(refuse("parse must return a table with metrics, card and details"));
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
        presentation: Presentation { metrics: parsed, card, details },
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
        Timestamp::from_unix(seconds).map_err(|_| refuse("a window's resets_at is not a plausible time"))?;
    }
    let length_secs = match integer(&table, "length_seconds")? {
        None => None,
        Some(seconds) if seconds > 0 => Some(seconds as u64),
        Some(_) => return Err(refuse("a window's length_seconds must be positive")),
    };
    Ok(Some(MetricWindow { key, resets_at, length_secs }))
}

/// The domain window a windowed metric produces.
fn window(metric: &Metric) -> Result<Option<Window>, PluginError> {
    let Some(declared) = metric.window.as_ref() else {
        return Ok(None);
    };
    let used_percent = metric.used_percent.filter(|used| used.is_finite()).ok_or_else(|| {
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
        resets_at: declared.resets_at.and_then(|s| Timestamp::from_unix(s).ok()),
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
            return Err(refuse(format!("every {field} item must come from gauge, value, ratio or status")));
        };
        let kind = text(&entry, MARKER, limits::MAX_ID_BYTES)?
            .and_then(|kind| WidgetKind::from_wire(&kind))
            .ok_or_else(|| refuse(format!("every {field} item must come from gauge, value, ratio or status")))?;
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
            format: enum_name::<tidemark_types::Format>(&entry, "format")?,
            emphasis: enum_name::<tidemark_types::Emphasis>(&entry, "emphasis")?,
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
                (_, Some(_)) => Err(refuse(format!("the gauge over {id} has no numerator"))),
                _ => Err(refuse(format!(
                    "the gauge over {id} needs a finite used_percent, or a numerator and a \
                     positive denominator"
                ))),
            }
        }
        WidgetKind::Ratio => {
            let (left, right) = (
                widget.left().ok_or_else(|| refuse(format!("the ratio over {id} needs a left field")))?,
                widget.right().ok_or_else(|| refuse(format!("the ratio over {id} needs a right field")))?,
            );
            match (metric.field(left), metric.field(right)) {
                (Some(_), Some(_)) => Ok(()),
                _ => Err(refuse(format!("the ratio over {id} is missing an operand"))),
            }
        }
        WidgetKind::Value => match widget.field().unwrap_or(Field::Value) {
            Field::Text => metric
                .text
                .as_ref()
                .map(|_| ())
                .ok_or_else(|| refuse(format!("the value over {id} reads text it does not have"))),
            selected => metric
                .field(selected)
                .map(|_| ())
                .ok_or_else(|| refuse(format!("the value over {id} reads {selected}, which is absent"))),
        },
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
            let text = text.to_str().map_err(lua_shape)?.to_owned();
            if text.len() > limit {
                return Err(refuse(format!("{field} is longer than {limit} bytes")));
            }
            Ok(Some(text))
        }
        other => Err(refuse(format!("{field} must be a string, not a {}", other.type_name()))),
    }
}

/// A finite numeric field.
fn number(table: &Table, field: &str) -> Result<Option<f64>, PluginError> {
    match table.get::<Value>(field).map_err(lua_shape)? {
        Value::Nil => Ok(None),
        Value::Integer(integer) => Ok(Some(integer as f64)),
        Value::Number(number) if number.is_finite() => Ok(Some(number)),
        Value::Number(_) => Err(refuse(format!("{field} is not a finite number"))),
        other => Err(refuse(format!("{field} must be a number, not a {}", other.type_name()))),
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
    match text(table, key, limits::MAX_ID_BYTES)? {
        None => Ok(None),
        Some(name) => Field::from_wire(&name)
            .map(|field| Some(field.as_wire().to_owned()))
            .ok_or_else(|| refuse(format!("{name} is not a metric field"))),
    }
}

/// A named choice from one of the presentation enums.
fn enum_name<T: WireEnum>(table: &Table, key: &str) -> Result<Option<String>, PluginError> {
    match text(table, key, limits::MAX_ID_BYTES)? {
        None => Ok(None),
        Some(name) => T::from_wire(&name)
            .map(|value| Some(value.as_wire().to_owned()))
            .ok_or_else(|| refuse(format!("{name} is not a {key} this build knows"))),
    }
}

/// The two enums `enum_name` reads, so it can be written once.
trait WireEnum: Sized + Copy {
    fn from_wire(value: &str) -> Option<Self>;
    fn as_wire(self) -> &'static str;
}

impl WireEnum for tidemark_types::Format {
    fn from_wire(value: &str) -> Option<Self> {
        Self::from_wire(value)
    }
    fn as_wire(self) -> &'static str {
        self.as_wire()
    }
}

impl WireEnum for tidemark_types::Emphasis {
    fn from_wire(value: &str) -> Option<Self> {
        Self::from_wire(value)
    }
    fn as_wire(self) -> &'static str {
        self.as_wire()
    }
}

/// Refuses a table that nests deeply enough to be a cycle. A cyclic table has no depth, so
/// this is also the cycle check: a loop hits the bound before anything recurses far enough
/// to matter.
fn depth_ok(table: &Table, depth: usize) -> Result<(), PluginError> {
    if depth > MAX_DEPTH {
        return Err(refuse("a returned table nests too deeply, or refers to itself"));
    }
    for entry in table.clone().pairs::<Value, Value>() {
        let (_, value) = entry.map_err(lua_shape)?;
        match value {
            Value::Table(nested) => depth_ok(&nested, depth + 1)?,
            Value::Function(_) | Value::Thread(_) | Value::UserData(_) | Value::LightUserData(_) => {
                return Err(refuse("a reading may not contain a function, a thread or userdata"));
            }
            _ => {}
        }
    }
    Ok(())
}

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
```

- [x] **Step 4: Run the tests**

Run: `cargo test -p tidemark-core plugin::output`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add crates/tidemark-core/src/plugin/output.rs
git commit -m "feat(core): validate a plugin's result into typed readings"
```

---

### Task 10: The generic plugin provider

**Files:**
- Create: `crates/tidemark-core/src/plugin/provider.rs`
- Test: colocated (transport tests are Task 18)

**Interfaces:**
- Consumes: `Definition`, `Method`, `lua::run`, `output::validate`, `limits::RESPONSE_BYTES`, `providers::{Provider, ProviderError, Credential, http}`.
- Produces:
  - `plugin::provider::Endpoint { url: String, allow_insecure_http: bool }`
  - `plugin::provider::PluginProvider::new(definition: Arc<Definition>, account: AccountId, endpoint: Endpoint, credential: Credential) -> Result<Self, ProviderError>`
  - `impl Provider for PluginProvider`
  - `plugin::provider::render(definition: &Definition, response: &[u8], account: &AccountId, captured_at: Timestamp) -> Result<Reading, PluginError>` — the fixture path `plugin render` uses.

- [x] **Step 1: Write the failing tests**

Create `crates/tidemark-core/src/plugin/provider.rs` with the tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::schema;

    const FILE: &str = r#"
format_version = 1

[provider]
id = "com.acme.quota"
name = "Acme AI"
plugin_version = "1.0.0"

[request]
method = "GET"
api_key_header = "X-Acme-Key"
api_key_prefix = ""

[parser]
language = "lua54"
source = '''
function parse(response, context)
    return {
        metrics = {
            {
                id = "cost",
                title = "Cost",
                value = number(response.used),
                maximum = number(response.limit),
                used_percent = percent(number(response.used), number(response.limit)),
                unit = "USD",
                window = { key = "monthly", length_seconds = 2592000 },
            },
        },
        card = { gauge("cost", { field = "used_percent" }) },
        details = {},
    }
end
'''
"#;

    fn definition() -> Arc<Definition> {
        Arc::new(schema::parse(FILE.as_bytes(), &[]).expect("the fixture parses"))
    }

    fn at() -> Timestamp {
        Timestamp::from_unix(1_788_870_896).expect("plausible")
    }

    #[test]
    fn a_fixture_renders_without_a_key_or_a_network() {
        let reading = render(
            &definition(),
            br#"{"used": "12.5", "limit": 50}"#,
            &AccountId::default(),
            at(),
        )
        .expect("the pure path needs nothing but the file and the body");
        assert_eq!(reading.presentation.metric("cost").unwrap().value, Some(12.5));
        assert_eq!(reading.snapshot.windows[0].used_percent, 25.0);
    }

    #[test]
    fn a_body_that_is_not_json_fails_at_the_json_stage() {
        let error = render(&definition(), b"<html>nope</html>", &AccountId::default(), at())
            .expect_err("this is not a JSON document");
        assert!(
            matches!(error, PluginError::Unreadable { .. }),
            "a body that is not JSON fails before Lua ever sees it: {error:?}"
        );
    }

    #[test]
    fn an_oversized_body_is_refused_before_it_is_parsed() {
        let huge = vec![b' '; limits::RESPONSE_BYTES + 1];
        assert!(matches!(
            render(&definition(), &huge, &AccountId::default(), at()),
            Err(PluginError::TooLarge { what: "the response body", .. })
        ));
    }

    #[test]
    fn an_endpoint_must_be_an_absolute_url_without_credentials_or_a_fragment() {
        for url in [
            "not a url",
            "/relative/path",
            "ftp://example.test/usage",
            "https://user:pass@example.test/usage",
            "https://example.test/usage#frag",
            "https://example.test/usage?key=abc#frag",
        ] {
            let endpoint = Endpoint { url: url.to_owned(), allow_insecure_http: false };
            assert!(
                PluginProvider::new(definition(), AccountId::default(), endpoint, Credential::new("k")).is_err(),
                "{url} must be refused"
            );
        }
        let good = Endpoint { url: "https://example.test/usage?window=month".into(), allow_insecure_http: false };
        assert!(PluginProvider::new(definition(), AccountId::default(), good, Credential::new("k")).is_ok());
    }

    #[test]
    fn plain_http_needs_the_accounts_acknowledgement() {
        let unacknowledged = Endpoint { url: "http://metrics.corp.test/usage".into(), allow_insecure_http: false };
        let error = PluginProvider::new(definition(), AccountId::default(), unacknowledged, Credential::new("k"))
            .expect_err("a key on the wire in clear needs saying so");
        assert!(error.to_string().contains("http"), "{error}");

        let acknowledged = Endpoint { url: "http://metrics.corp.test/usage".into(), allow_insecure_http: true };
        assert!(
            PluginProvider::new(definition(), AccountId::default(), acknowledged, Credential::new("k")).is_ok()
        );
    }

    #[test]
    fn an_account_with_no_key_cannot_be_built_into_a_polling_client() {
        let endpoint = Endpoint { url: "https://example.test/usage".into(), allow_insecure_http: false };
        assert!(
            PluginProvider::new(definition(), AccountId::default(), endpoint, Credential::new("   ")).is_err()
        );
    }

    #[test]
    fn the_built_request_carries_the_declared_header_and_tidemarks_own_identity() {
        let endpoint = Endpoint { url: "https://example.test/usage".into(), allow_insecure_http: false };
        let provider =
            PluginProvider::new(definition(), AccountId::default(), endpoint, Credential::new("sk-test"))
                .expect("builds");
        let request = provider.build_request().expect("builds a request");
        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(request.headers()["x-acme-key"], "sk-test");
        assert_eq!(request.headers()["accept"], "application/json");
        assert_eq!(request.headers()["user-agent"], tidemark_types::user_agent());
        assert!(request.body().is_none());
    }

    #[test]
    fn a_post_declaration_sends_an_empty_body() {
        let file = FILE.replace("method = \"GET\"", "method = \"POST\"");
        let definition = Arc::new(schema::parse(file.as_bytes(), &[]).expect("parses"));
        let endpoint = Endpoint { url: "https://example.test/usage".into(), allow_insecure_http: false };
        let provider = PluginProvider::new(definition, AccountId::default(), endpoint, Credential::new("k"))
            .expect("builds");
        let request = provider.build_request().expect("builds");
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request.body().and_then(reqwest::Body::as_bytes).unwrap_or_default(),
            b"",
            "a plugin has no way to send a body"
        );
    }

    #[test]
    fn a_prefix_is_placed_in_front_of_the_key_verbatim() {
        let file = FILE
            .replace("api_key_header = \"X-Acme-Key\"", "api_key_header = \"Authorization\"")
            .replace("api_key_prefix = \"\"", "api_key_prefix = \"Bearer \"");
        let definition = Arc::new(schema::parse(file.as_bytes(), &[]).expect("parses"));
        let endpoint = Endpoint { url: "https://example.test/usage".into(), allow_insecure_http: false };
        let provider = PluginProvider::new(definition, AccountId::default(), endpoint, Credential::new("sk-1"))
            .expect("builds");
        assert_eq!(
            provider.build_request().expect("builds").headers()["authorization"],
            "Bearer sk-1"
        );
    }

    #[test]
    fn the_provider_reports_the_plugin_id_and_account_it_polls() {
        let endpoint = Endpoint { url: "https://example.test/usage".into(), allow_insecure_http: false };
        let provider = PluginProvider::new(
            definition(),
            AccountId::new("work"),
            endpoint,
            Credential::new("k"),
        )
        .expect("builds");
        assert_eq!(provider.id().as_str(), "com.acme.quota");
        assert_eq!(provider.account().as_str(), "work");
    }

    #[test]
    fn a_plugin_failure_is_reported_as_a_malformed_reading_and_names_no_secret() {
        let file = FILE.replace("return {", "error(\"boom \" .. tostring(response.used)) return {");
        let definition = Arc::new(schema::parse(file.as_bytes(), &[]).expect("parses"));
        let error = render(&definition, br#"{"used": 1, "limit": 2}"#, &AccountId::default(), at())
            .expect_err("the plugin raised");
        let rendered = error.to_string();
        assert!(rendered.contains("boom"));
        assert!(!rendered.contains("sk-"), "no credential can appear in a diagnostic: {rendered}");
    }

    #[test]
    fn a_plugin_error_maps_onto_the_provider_failure_the_engine_already_handles() {
        assert!(matches!(
            provider_error(PluginError::Output { reason: "no".into() }),
            ProviderError::Malformed(_)
        ));
        assert!(matches!(
            provider_error(PluginError::LuaExhausted { what: "instruction" }),
            ProviderError::Malformed(_)
        ));
    }
}
```

- [x] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemark-core plugin::provider`
Expected: FAIL — `cannot find type PluginProvider`.

- [x] **Step 3: Write the provider**

```rust
//! One plugin account, as something the engine can poll.
//!
//! The transport is Tidemark's and is the same for every plugin: one request to the URL the
//! *account owner* typed, the key in the header the file declared, redirects off, the body
//! bounded, then a pure transformation. A plugin cannot choose a host, cannot add a header,
//! cannot follow a redirect to another origin and cannot see the credential — which is what
//! makes importing a file from a stranger a reasonable thing to do.

use super::{Definition, Method, PluginError, Reading, limits, lua, output};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http};
use std::sync::Arc;
use tidemark_types::{AccountId, ProviderId, Snapshot, Timestamp};

/// Where one account sends its request. Supplied by the account owner, never by the plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The absolute URL the key is sent to.
    pub url: String,
    /// Whether the owner acknowledged that this URL puts the key on the network in clear.
    /// Only ever true for `http://`, and the plugin cannot ask for it or remember it.
    pub allow_insecure_http: bool,
}

/// One plugin account.
pub struct PluginProvider {
    definition: Arc<Definition>,
    account: AccountId,
    client: reqwest::Client,
    url: reqwest::Url,
    credential: Credential,
}

impl std::fmt::Debug for PluginProvider {
    /// By hand: the credential must never reach a log, and `Definition` carries a whole file.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginProvider")
            .field("provider", &self.definition.id)
            .field("account", &self.account)
            .field("url", &self.url.as_str())
            .finish_non_exhaustive()
    }
}

impl PluginProvider {
    /// Builds a client, refusing every endpoint the format does not allow.
    pub fn new(
        definition: Arc<Definition>,
        account: AccountId,
        endpoint: Endpoint,
        credential: Credential,
    ) -> Result<Self, ProviderError> {
        if credential.is_blank() {
            return Err(ProviderError::NoCredential);
        }
        let url = reqwest::Url::parse(&endpoint.url)
            .map_err(|error| ProviderError::Local(format!("the endpoint is not a URL: {error}")))?;
        match url.scheme() {
            "https" => {}
            "http" if endpoint.allow_insecure_http => {}
            "http" => {
                return Err(ProviderError::Local(
                    "this endpoint is plain http, which puts the API key on the network in \
                     clear; confirm insecure transport for this account to use it"
                        .to_owned(),
                ));
            }
            other => {
                return Err(ProviderError::Local(format!(
                    "{other} endpoints are not supported; use https"
                )));
            }
        }
        if url.host().is_none() {
            return Err(ProviderError::Local("the endpoint has no host".to_owned()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ProviderError::Local(
                "an endpoint may not carry credentials in its URL".to_owned(),
            ));
        }
        if url.fragment().is_some() {
            return Err(ProviderError::Local(
                "an endpoint may not carry a fragment".to_owned(),
            ));
        }

        // Redirects off: a 302 would move the secret header to whatever origin answered.
        let client = http::builder()
            .map_err(ProviderError::Client)?
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(ProviderError::Client)?;

        Ok(Self { definition, account, client, url, credential })
    }

    /// The one request, built. Separate from [`Provider::fetch`] so its headers are testable
    /// without a server: the header the key goes in is the whole security boundary.
    pub(crate) fn build_request(&self) -> Result<reqwest::Request, ProviderError> {
        let method = match self.definition.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
        };
        let mut builder = self
            .client
            .request(method, self.url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                &self.definition.api_key_header,
                format!("{}{}", self.definition.api_key_prefix, self.credential.expose()),
            );
        if self.definition.method == Method::Post {
            builder = builder.body(reqwest::Body::from(Vec::new()));
        }
        builder
            .build()
            .map_err(|error| ProviderError::Local(format!("the request could not be built: {error}")))
    }
}

impl Provider for PluginProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new(self.definition.id.clone())
    }

    fn account(&self) -> AccountId {
        self.account.clone()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(async move {
            let request = self.build_request()?;
            let body = crate::providers::keyed::request(&self.definition.id, &self.client, request).await?;
            let reading = render(
                &self.definition,
                body.as_bytes(),
                &self.account,
                Timestamp::now(),
            )
            .map_err(provider_error)?;
            Ok(reading.snapshot)
        })
    }
}

/// One reading, and its layout, kept together for the caller that needs both.
///
/// [`Provider::fetch`] can only return a `Snapshot`, so the daemon calls this instead when it
/// wants the presentation too — see `registry::plugin_account`.
pub async fn poll(provider: &PluginProvider) -> Result<Reading, ProviderError> {
    let request = provider.build_request()?;
    let body =
        crate::providers::keyed::request(&provider.definition.id, &provider.client, request).await?;
    render(
        &provider.definition,
        body.as_bytes(),
        &provider.account,
        Timestamp::now(),
    )
    .map_err(provider_error)
}

/// The pure path: a body, a definition and a timestamp, with no key and no network.
///
/// This is what `tidemarkctl plugin render` runs, and it is the documented authoring loop —
/// so it is the same function polling uses, not a second implementation that could disagree.
pub fn render(
    definition: &Definition,
    response: &[u8],
    account: &AccountId,
    captured_at: Timestamp,
) -> Result<Reading, PluginError> {
    if response.len() > limits::RESPONSE_BYTES {
        return Err(PluginError::TooLarge {
            what: "the response body",
            found: response.len(),
            limit: limits::RESPONSE_BYTES,
        });
    }
    let document: serde_json::Value =
        serde_json::from_slice(response).map_err(|error| PluginError::Unreadable {
            path_hint: "the response".to_owned(),
            reason: error.to_string(),
        })?;
    let executed = lua::run(&definition.lua_source, &document, captured_at.as_unix())?;
    output::validate(
        &executed,
        &ProviderId::new(definition.id.clone()),
        account,
        captured_at,
    )
}

/// A plugin failure as the state machine the engine already has.
///
/// Every plugin failure is `Malformed`: the response arrived and did not mean what the
/// account's definition says it means. That is the state whose remedy is `TheyBrokeIt`, which
/// is the truth — either the endpoint changed or the file is wrong, and neither is something
/// the user fixes by waiting or by pasting a new key.
pub fn provider_error(error: PluginError) -> ProviderError {
    ProviderError::Malformed(error.to_string())
}
```

- [x] **Step 4: Run the tests, then restore the module list**

Run: `cargo test -p tidemark-core plugin`
Expected: PASS. Uncomment every `pub mod` line in `plugin/mod.rs` — all five modules now exist.

- [x] **Step 5: Run the gate and commit**

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
git add crates/tidemark-core/src/plugin
git commit -m "feat(core): one generic single-request plugin provider"
```

---

### Task 11: Per-account endpoint configuration

**Files:**
- Modify: `crates/tidemark-core/src/config.rs`
- Test: colocated

**Interfaces:**
- Consumes: `plugin::provider::Endpoint`, `ConfigError`.
- Produces:
  - `Config::plugin_endpoint(&self, provider: &str, account: &str) -> Result<Option<Endpoint>, ConfigError>`
  - `Config::set_plugin_endpoint(&mut self, provider: &str, account: &str, endpoint: &Endpoint) -> Result<(), ConfigError>`
  - `Config::remove_plugin_endpoint(&mut self, provider: &str, account: &str) -> Result<(), ConfigError>`
  - `ConfigError::InvalidEndpoint { path, provider, account, reason }`

- [ ] **Step 1: Write the failing tests**

In `crates/tidemark-core/src/config.rs`'s `mod tests`. That module has no seeded-config helper
and `Config` exposes no `path()`, so these use what is already there — `scratch(name)` for a path
and `Config::at(path)` to load — through one local helper that keeps the path in hand so the file
can be read back:

```rust
    /// A config over its own scratch file, seeded with `body`.
    fn seeded(name: &str, body: &str) -> (PathBuf, Config) {
        let path = scratch(name);
        std::fs::write(&path, body).expect("seed written");
        let config = Config::at(path.clone()).expect("loads");
        (path, config)
    }
```


```rust
    #[test]
    fn each_account_gets_its_own_endpoint() {
        let (path, mut config) = seeded("endpoints", "");
        config
            .set_plugin_endpoint(
                "com.acme.quota",
                "default",
                &Endpoint { url: "https://metrics.example.test/v1/usage".into(), allow_insecure_http: false },
            )
            .expect("writes");
        config
            .set_plugin_endpoint(
                "com.acme.quota",
                "work",
                &Endpoint { url: "http://metrics.corp.test/v1/usage".into(), allow_insecure_http: true },
            )
            .expect("writes");

        let default = config.plugin_endpoint("com.acme.quota", "default").expect("reads").expect("set");
        let work = config.plugin_endpoint("com.acme.quota", "work").expect("reads").expect("set");
        assert_eq!(default.url, "https://metrics.example.test/v1/usage");
        assert!(!default.allow_insecure_http);
        assert_eq!(work.url, "http://metrics.corp.test/v1/usage");
        assert!(work.allow_insecure_http, "one account's acknowledgement is not the other's");
    }

    #[test]
    fn an_endpoint_survives_a_reload_under_the_account_addressed_table() {
        let (path, mut config) = seeded("endpoint-reload", "");
        let endpoint = Endpoint { url: "https://a.test/u".into(), allow_insecure_http: false };
        config.set_plugin_endpoint("com.acme.quota", "work", &endpoint).expect("writes");
        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(
            text.contains("[provider.\"com.acme.quota\".account.work]"),
            "the endpoint is account-addressed, not a shared provider option: {text}"
        );
        let reloaded = Config::at(path).expect("reloads");
        assert_eq!(reloaded.plugin_endpoint("com.acme.quota", "work").unwrap(), Some(endpoint));
    }

    #[test]
    fn an_endpoint_write_preserves_the_rest_of_the_file_and_its_decoration() {
        let (path, mut config) = seeded(
            "endpoint-decoration",
            "# kept\nproviders = [\"zai\"]\n\n[provider.zai]\n# a comment\nregion = \"global\"\n",
        );
        config
            .set_plugin_endpoint(
                "com.acme.quota",
                "default",
                &Endpoint { url: "https://a.test/u".into(), allow_insecure_http: false },
            )
            .expect("writes");
        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(text.contains("# kept"));
        assert!(text.contains("# a comment"));
        assert!(text.contains("region = \"global\""));
    }

    #[test]
    fn a_present_but_invalid_endpoint_is_refused_rather_than_ignored() {
        for body in [
            "[provider.\"com.acme.quota\".account.default]\nendpoint = 4\n",
            "[provider.\"com.acme.quota\".account.default]\nendpoint = \"https://a.test\"\nallow_insecure_http = \"yes\"\n",
            "[provider.\"com.acme.quota\".account.default]\nallow_insecure_http = true\n",
        ] {
            let (_path, config) = seeded("endpoint-invalid", body);
            assert!(
                matches!(
                    config.plugin_endpoint("com.acme.quota", "default"),
                    Err(ConfigError::InvalidEndpoint { .. })
                ),
                "{body} must be refused"
            );
        }
    }

    #[test]
    fn an_account_with_no_endpoint_reads_as_none_rather_than_an_error() {
        let (_path, config) = seeded("endpoint-absent", "");
        assert_eq!(config.plugin_endpoint("com.acme.quota", "default").expect("reads"), None);
    }

    #[test]
    fn removing_an_account_removes_its_endpoint_and_its_acknowledgement() {
        let (path, mut config) = seeded("endpoint-account-removal", "");
        config.add_provider("com.acme.quota").expect("adds");
        config.set_accounts("com.acme.quota", &["default".into(), "work".into()]).expect("sets");
        for account in ["default", "work"] {
            config
                .set_plugin_endpoint(
                    "com.acme.quota",
                    account,
                    &Endpoint { url: "http://a.test/u".into(), allow_insecure_http: true },
                )
                .expect("writes");
        }
        config.remove_account("com.acme.quota", "work").expect("removes");
        assert_eq!(config.plugin_endpoint("com.acme.quota", "work").expect("reads"), None);
        assert!(config.plugin_endpoint("com.acme.quota", "default").expect("reads").is_some());
        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(!text.contains("account.work"), "{text}");
    }

    #[test]
    fn removing_a_provider_removes_every_accounts_endpoint() {
        let (path, mut config) = seeded("endpoint-provider-removal", "");
        config.add_provider("com.acme.quota").expect("adds");
        config
            .set_plugin_endpoint(
                "com.acme.quota",
                "default",
                &Endpoint { url: "https://a.test/u".into(), allow_insecure_http: false },
            )
            .expect("writes");
        config.remove_provider("com.acme.quota").expect("removes");
        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(!text.contains("com.acme.quota"), "{text}");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemark-core config::tests::`
Expected: FAIL — `no method named plugin_endpoint`.

- [ ] **Step 3: Add the error variant**

In `ConfigError`:

```rust
    /// A plugin account's endpoint is present and cannot be used.
    #[error("{path}: [{PROVIDER_TABLE}.{provider}.account.{account}] {reason}")]
    InvalidEndpoint {
        /// The file.
        path: PathBuf,
        /// Whose endpoint it is.
        provider: String,
        /// Which account's.
        account: String,
        /// What was wrong with it.
        reason: String,
    },
```

- [ ] **Step 4: Implement the three operations**

In `impl Config`, with this doc comment — the reasoning is the reason the path is not a `ProviderOption`:

```rust
    /// One plugin account's endpoint, or `None` when it has not been configured.
    ///
    /// Account-addressed, and deliberately not a [`Self::option`]: a provider option is one
    /// value under `[provider.<id>]` shared by every account, and `Engine::set_option` takes
    /// an account only to decide which client to rebuild. An endpoint stored that way would
    /// silently send two accounts' different keys to one host — which is the single thing this
    /// feature must never do.
    ///
    /// Present-but-invalid is refused rather than ignored: an endpoint the file holds in a
    /// shape this build cannot read must not fall back to "unconfigured", because a
    /// half-configured account would then quietly stop polling.
    pub fn plugin_endpoint(
        &self,
        provider: &str,
        account: &str,
    ) -> Result<Option<crate::plugin::provider::Endpoint>, ConfigError> {
        let Some(table) = self
            .document
            .get(PROVIDER_TABLE)
            .and_then(|table| table.get(provider))
            .and_then(|table| table.get(ACCOUNT_TABLE))
            .and_then(|table| table.get(account))
        else {
            return Ok(None);
        };
        let invalid = |reason: &str| ConfigError::InvalidEndpoint {
            path: self.path.clone(),
            provider: provider.to_owned(),
            account: account.to_owned(),
            reason: reason.to_owned(),
        };
        let table = table.as_table_like().ok_or_else(|| invalid("must be a table"))?;
        let url = match table.get(ENDPOINT_KEY) {
            None => return Ok(None),
            Some(item) => item
                .as_str()
                .ok_or_else(|| invalid("endpoint must be a string"))?
                .to_owned(),
        };
        if url.trim().is_empty() {
            return Err(invalid("endpoint must not be empty"));
        }
        let allow_insecure_http = match table.get(INSECURE_KEY) {
            None => false,
            Some(item) => item
                .as_bool()
                .ok_or_else(|| invalid("allow_insecure_http must be true or false"))?,
        };
        Ok(Some(crate::plugin::provider::Endpoint { url, allow_insecure_http }))
    }

    /// Stores one plugin account's endpoint and writes the file.
    pub fn set_plugin_endpoint(
        &mut self,
        provider: &str,
        account: &str,
        endpoint: &crate::plugin::provider::Endpoint,
    ) -> Result<(), ConfigError> {
        let table = self.account_table(provider, account)?;
        table.insert(ENDPOINT_KEY, value(&endpoint.url));
        table.insert(INSECURE_KEY, Item::Value(Value::from(endpoint.allow_insecure_http)));
        self.write()
    }

    /// Removes one plugin account's endpoint and acknowledgement, and writes the file.
    pub fn remove_plugin_endpoint(&mut self, provider: &str, account: &str) -> Result<(), ConfigError> {
        let Some(accounts) = self
            .document
            .get_mut(PROVIDER_TABLE)
            .and_then(|table| table.get_mut(provider))
            .and_then(|table| table.get_mut(ACCOUNT_TABLE))
            .and_then(|table| table.as_table_like_mut())
        else {
            return Ok(());
        };
        accounts.remove(account);
        self.write()
    }

    /// The `[provider.<id>.account.<account>]` table, created if it is not there.
    fn account_table(
        &mut self,
        provider: &str,
        account: &str,
    ) -> Result<&mut dyn toml_edit::TableLike, ConfigError> {
        // The same entry/`as_table_like_mut` walk `set_option_value` uses, one level deeper, so a
        // non-table in the way is the same `NotATable` refusal rather than a second spelling of
        // it — and every intermediate table is created implicitly, which is what preserves the
        // decoration around whatever is already in the file.
        self.normalize_providers(None)?;
        let mut table = self
            .document
            .entry(PROVIDER_TABLE)
            .or_insert_with(|| Item::Table(implicit_table()))
            .as_table_like_mut()
            .ok_or_else(|| ConfigError::NotATable {
                path: self.path.clone(),
                table: PROVIDER_TABLE.to_owned(),
            })?;
        for (segment, dotted) in [
            (provider, provider.to_owned()),
            (ACCOUNT_TABLE, format!("{provider}.{ACCOUNT_TABLE}")),
            (account, format!("{provider}.{ACCOUNT_TABLE}.{account}")),
        ] {
            table = table
                .entry(segment)
                .or_insert_with(|| Item::Table(Table::new()))
                .as_table_like_mut()
                .ok_or_else(|| ConfigError::NotATable {
                    path: self.path.clone(),
                    table: format!("{PROVIDER_TABLE}.{dotted}"),
                })?;
        }
        Ok(table)
    }
```

Add the two key constants beside `ACCOUNTS_KEY`:

```rust
/// The subtable a plugin provider's per-account configuration lives under.
const ACCOUNT_TABLE: &str = "account";
/// The endpoint key inside it.
const ENDPOINT_KEY: &str = "endpoint";
/// The insecure-transport acknowledgement key inside it.
const INSECURE_KEY: &str = "allow_insecure_http";
```

- [ ] **Step 5: Extend account and provider removal**

In `Config::remove_account`, before `set_accounts`, remove the account's endpoint table entry; in `Config::remove_provider`, the existing removal of `[provider.<id>]` already carries the `account` subtable away — assert that with the test written in Step 1 rather than adding code.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p tidemark-core config`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/tidemark-core/src/config.rs
git commit -m "feat(core): account-addressed plugin endpoint configuration"
```

---

### Task 12: The installed-definition store

**Files:**
- Create: `crates/tidemarkd/src/plugins.rs`
- Modify: `crates/tidemarkd/src/main.rs` (`mod plugins;`)
- Modify: `crates/tidemark-core/src/paths.rs` (`plugins_dir`)
- Test: colocated in `plugins.rs`

**Interfaces:**
- Consumes: `plugin::{Definition, PluginError, schema}`, `paths`.
- Produces:
  - `paths::plugins_dir() -> Result<PathBuf, NoBaseDirectory>` — `<data_dir>/plugins`
  - `paths::plugin_icons_dir() -> Result<PathBuf, NoBaseDirectory>` — `<data_dir>/plugins/icons`
  - `plugins::Store::open(root: PathBuf) -> Result<Self, StoreError>`
  - `Store::inspect(&self, bytes: &[u8], reserved: &[&str]) -> Result<Definition, PluginError>`
  - `Store::install(&mut self, bytes: &[u8], reserved: &[&str]) -> Result<Definition, StoreError>`
  - `Store::installed(&self) -> Vec<Arc<Definition>>`
  - `Store::get(&self, id: &str) -> Option<Arc<Definition>>`
  - `Store::remove(&mut self, id: &str, configured_accounts: usize) -> Result<(), StoreError>`
  - `StoreError` (`InUse { id, accounts }`, `Io`, `Plugin`)

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Task 5's minimal file, plus a mark, so the store's icon materialization is covered.
    const FILE: &str = r#"
format_version = 1

[provider]
id = "com.acme.quota"
name = "Acme AI"
plugin_version = "1.0.0"

[request]
method = "GET"
api_key_header = "X-Acme-Key"
api_key_prefix = ""

[parser]
language = "lua54"
source = '''
function parse(response, context)
    return { metrics = {}, card = {}, details = {} }
end
'''

[icon]
svg = '''
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><path fill="currentColor" d="M0 0h64v64H0z"/></svg>
'''
"#;

    /// A store over its own scratch root. The workspace depends on no temp-directory crate —
    /// its tests build paths under `std::env::temp_dir()` with the process id in the name — so
    /// this does the same rather than adding one.
    fn store(name: &str) -> (PathBuf, Store) {
        let root = std::env::temp_dir()
            .join(format!("tidemark-plugins-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch root");
        (root.clone(), Store::open(root).expect("opens"))
    }

    #[test]
    fn inspecting_a_file_validates_it_and_writes_nothing() {
        let (root, store) = store("inspect");
        let definition = store.inspect(FILE.as_bytes(), &[]).expect("valid");
        assert_eq!(definition.id, "com.acme.quota");
        assert_eq!(
            std::fs::read_dir(&root).expect("readable").count(),
            0,
            "inspection is a dry run: nothing is stored and nothing is polled"
        );
    }

    #[test]
    fn installing_writes_the_exact_validated_bytes_under_the_stable_id() {
        let (root, mut store) = store("install");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        let path = root.join("com.acme.quota.tidemark-provider");
        assert_eq!(std::fs::read(&path).expect("stored"), FILE.as_bytes());
        assert_eq!(store.installed().len(), 1);
        assert!(store.get("com.acme.quota").is_some());
    }

    #[test]
    fn a_rejected_replacement_leaves_the_installed_definition_untouched() {
        let (_root, mut store) = store("rejected-replacement");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        let broken = FILE.replace("method = \"GET\"", "method = \"PATCH\"");
        assert!(store.install(broken.as_bytes(), &[]).is_err());
        let kept = store.get("com.acme.quota").expect("still installed");
        assert_eq!(kept.method.as_str(), "GET", "import is transactional");
    }

    #[test]
    fn replacing_a_definition_keeps_its_id_and_replaces_its_bytes() {
        let (_root, mut store) = store("replacement");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        let updated = FILE.replace("plugin_version = \"1.0.0\"", "plugin_version = \"1.1.0\"");
        store.install(updated.as_bytes(), &[]).expect("replaces");
        assert_eq!(store.installed().len(), 1, "a replacement is not a second provider");
        assert_eq!(store.get("com.acme.quota").unwrap().plugin_version, "1.1.0");
    }

    #[test]
    fn a_definition_with_accounts_configured_cannot_be_removed() {
        let (_root, mut store) = store("in-use");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        assert!(matches!(
            store.remove("com.acme.quota", 2),
            Err(StoreError::InUse { accounts: 2, .. })
        ));
        assert!(store.get("com.acme.quota").is_some(), "the refusal changed nothing");
        store.remove("com.acme.quota", 0).expect("removable once no account uses it");
        assert!(store.get("com.acme.quota").is_none());
    }

    #[test]
    fn a_parser_that_does_not_compile_is_refused_at_import_rather_than_at_the_first_poll() {
        let (_root, mut store) = store("compile");
        let broken = FILE.replace("function parse(response, context)", "function parse( then");
        assert!(matches!(
            store.inspect(broken.as_bytes(), &[]),
            Err(PluginError::LuaCompile { .. })
        ));
        assert!(store.install(broken.as_bytes(), &[]).is_err());
        assert!(store.get("com.acme.quota").is_none(), "and nothing was stored");
    }

    #[test]
    fn a_reserved_or_colliding_id_is_refused_at_install_time_too() {
        let (_root, mut store) = store("reserved");
        assert!(store.install(FILE.as_bytes(), &["com.acme.quota"]).is_err());
    }

    #[test]
    fn opening_a_store_reads_every_installed_definition_and_skips_the_unreadable_ones() {
        let (root, mut store) = store("reopen");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        std::fs::write(root.join("junk.tidemark-provider"), b"not toml").expect("written");
        std::fs::write(root.join("notes.txt"), b"ignored").expect("written");
        let reopened = Store::open(root.clone()).expect("opens");
        assert_eq!(
            reopened.installed().len(),
            1,
            "one bad file must not stop the daemon reading the good ones"
        );
    }

    #[test]
    fn a_definitions_mark_is_materialized_where_the_icon_theme_finds_it() {
        let (root, mut store) = store("mark");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        let icon = root
            .path()
            .join("icons/hicolor/symbolic/apps/tidemark-com-acme-quota-symbolic.svg");
        assert!(icon.exists(), "the GUI loads plugin marks through the icon theme, by name");
    }

    #[test]
    fn removing_a_definition_removes_its_mark() {
        let (root, mut store) = store("mark-removal");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        store.remove("com.acme.quota", 0).expect("removes");
        assert!(
            !root.join("icons/hicolor/symbolic/apps/tidemark-com-acme-quota-symbolic.svg").exists()
        );
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemarkd plugins`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Add the paths**

In `crates/tidemark-core/src/paths.rs`:

```rust
/// Where installed plugin definitions live: `<data_dir>/plugins`.
pub fn plugins_dir() -> Result<PathBuf, NoBaseDirectory> {
    Ok(data_dir()?.join("plugins"))
}

/// Where a plugin's sanitized mark is materialized so the icon theme can find it by name:
/// `<data_dir>/plugins/icons`, laid out as an XDG icon theme root. The GUI adds this one
/// directory to its search path, and then a plugin mark is looked up exactly the way a
/// shipped mark is — which is what keeps symbolic recolouring working. See `mark.rs`.
pub fn plugin_icons_dir() -> Result<PathBuf, NoBaseDirectory> {
    Ok(plugins_dir()?.join("icons"))
}
```

- [ ] **Step 4: Write the store**

Create `crates/tidemarkd/src/plugins.rs`:

```rust
//! Installed plugin definitions, on disk and in memory.
//!
//! Two rules shape this. **Import is transactional**: a file is validated, and only then are
//! its exact bytes written, staged-and-renamed, so a rejected replacement leaves the previous
//! definition installed and pollable. **The id is the storage key**: the file is named from
//! it, accounts and keys hang off it, and a definition with configured accounts cannot be
//! removed — the user removes the accounts first, with the removal semantics that already
//! delete their credentials.
//!
//! The original validated bytes are stored unchanged, so a definition stays inspectable and
//! exportable; only the sanitized mark is materialized separately, because the icon theme
//! loads marks from files by name.
```

with `Store` holding `root: PathBuf` and `installed: BTreeMap<String, Arc<Definition>>`. `inspect`
runs `plugin::schema::parse` **and then `plugin::lua::compile`**, so a file whose parser does not
compile is refused at import rather than at the first poll; `install` runs the same two checks and, only then writes `root/<id>.tidemark-provider` via a `.tmp` + `rename`, writing `Definition::icon_svg` to `root/icons/hicolor/symbolic/apps/tidemark-<slug>-symbolic.svg` using `tidemark_types::plugin_icon_slug`, and only then inserting into the map; `remove` refusing with `StoreError::InUse` when `configured_accounts > 0`, then deleting both files and the map entry; `open` reading `*.tidemark-provider`, parsing each, and `tracing::warn!`-ing past the ones that fail.

- [ ] **Step 5: Run the tests, then the gate**

Run: `cargo test -p tidemarkd plugins && ./scripts/check-layering.sh`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/tidemarkd/src/plugins.rs crates/tidemarkd/src/main.rs crates/tidemark-core/src/paths.rs
git commit -m "feat(daemon): store installed plugin definitions"
```

---

### Task 13: Dynamic registry entries

**Files:**
- Modify: `crates/tidemarkd/src/registry.rs`
- Modify: `crates/tidemark-types/src/wire.rs` (`ProviderDefinition::plugin`)
- Test: colocated in `registry.rs`

**Interfaces:**
- Consumes: Task 12's `Store`, Task 10's `PluginProvider`/`Endpoint`, Task 11's `Config::plugin_endpoint`.
- Produces:
  - `ProviderDefinition.plugin: Option<PluginInfo>` where `PluginInfo { id, name, plugin_version, method, api_key_header, api_key_prefix, has_mark }` (an `a{sv}` dict in `wire.rs`)
  - `registry::catalog_with_plugins(config: &Config, plugins: &[Arc<Definition>]) -> Vec<ProviderDefinition>`
  - `registry::plugin_account(definition: &Arc<Definition>, account: &AccountId, secrets: &Arc<dyn Secrets>, config: &Config) -> Result<Account, ProviderError>`
  - `registry::builtin_ids() -> Vec<&'static str>` — the reserved list `schema::parse` takes
  - `registry::accounts` and `registry::title` extended to consult the installed definitions

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn the_catalog_lists_installed_plugins_after_the_compiled_providers() {
        let config = empty_config();
        let plugins = vec![test_definition("com.acme.quota", "Acme AI")];
        let catalog = catalog_with_plugins(&config, &plugins);
        let last = catalog.last().expect("non-empty");
        assert_eq!(last.provider, "com.acme.quota");
        assert_eq!(last.title, "Acme AI");
        assert_eq!(last.credential, CredentialKind::Key.as_wire());
        let info = last.plugin.as_ref().expect("a plugin publishes what it declared");
        assert_eq!(info.method, "GET");
        assert_eq!(info.api_key_header, "X-Acme-Key");
        assert_eq!(info.api_key_prefix, "");
        assert!(info.has_mark);
    }

    #[test]
    fn a_compiled_provider_publishes_no_plugin_metadata() {
        let catalog = catalog_with_plugins(&empty_config(), &[]);
        assert!(catalog.iter().all(|definition| definition.plugin.is_none()));
    }

    #[test]
    fn every_built_in_id_is_reserved_against_a_plugin_claiming_it() {
        let ids = builtin_ids();
        for expected in ["zai", "claude", "codex", "antigravity", "nanogpt"] {
            assert!(ids.contains(&expected), "{expected} must be reserved");
        }
        assert_eq!(
            ids.len(),
            catalog_with_plugins(&empty_config(), &[]).len(),
            "the reserved list and the compiled catalog are the same set"
        );
    }

    #[test]
    fn a_plugin_account_is_built_from_its_configured_endpoint() {
        let mut config = empty_config();
        config.add_provider("com.acme.quota").expect("adds");
        config
            .set_plugin_endpoint(
                "com.acme.quota",
                "default",
                &Endpoint { url: "https://a.test/u".into(), allow_insecure_http: false },
            )
            .expect("writes");
        let secrets = fake_secrets_with_key("com.acme.quota", "default", "sk-1");
        let account = plugin_account(
            &test_definition("com.acme.quota", "Acme AI"),
            &AccountId::default(),
            &secrets,
            &config,
        )
        .expect("builds");
        assert_eq!(account.provider().as_str(), "com.acme.quota");
        assert_eq!(account.status().credential.as_deref(), Some("key"));
    }

    #[test]
    fn a_plugin_account_with_no_endpoint_publishes_what_is_missing_instead_of_polling() {
        let config = empty_config();
        let secrets = fake_secrets_with_key("com.acme.quota", "default", "sk-1");
        let account = plugin_account(
            &test_definition("com.acme.quota", "Acme AI"),
            &AccountId::default(),
            &secrets,
            &config,
        )
        .expect("an unconfigured account still exists");
        let message = account.status().message.clone().unwrap_or_default();
        assert!(message.contains("endpoint"), "the card says what to fill in: {message}");
    }

    #[test]
    fn a_configured_plugin_account_is_among_the_accounts_the_daemon_polls() {
        let mut config = empty_config();
        config.add_provider("com.acme.quota").expect("adds");
        config
            .set_plugin_endpoint(
                "com.acme.quota",
                "default",
                &Endpoint { url: "https://a.test/u".into(), allow_insecure_http: false },
            )
            .expect("writes");
        let secrets = fake_secrets_with_key("com.acme.quota", "default", "sk-1");
        let plugins = vec![test_definition("com.acme.quota", "Acme AI")];
        let accounts = accounts_with_plugins(&secrets, &config, &plugins).expect("builds");
        assert_eq!(accounts.len(), 1);
    }

    #[test]
    fn a_configured_provider_whose_definition_is_gone_is_warned_about_not_polled() {
        let mut config = empty_config();
        config.add_provider("com.acme.quota").expect("adds");
        let secrets = fake_secrets();
        assert!(accounts_with_plugins(&secrets, &config, &[]).expect("builds").is_empty());
    }

    #[test]
    fn a_plugin_is_titled_by_its_own_name_in_notifications() {
        let plugins = vec![test_definition("com.acme.quota", "Acme AI")];
        assert_eq!(plugin_title("com.acme.quota", &plugins), Some("Acme AI"));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemarkd registry`
Expected: FAIL — `cannot find function catalog_with_plugins`.

- [ ] **Step 3: Publish plugin metadata**

In `crates/tidemark-types/src/wire.rs`, add and export:

```rust
/// What an installed plugin declares about itself, for the import preview and the settings
/// pane.
///
/// Published rather than read from the file by the client: the client has no parser, and the
/// two facts a user must see before the first request — which header the key goes in, and
/// what goes in front of it — are exactly the ones a client must not have to guess at.
#[derive(Debug, Clone, PartialEq, Eq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct PluginInfo {
    /// Reverse-DNS provider id.
    pub id: String,
    /// The author's display name.
    pub name: String,
    /// The author's SemVer string. Informational.
    pub plugin_version: String,
    /// `GET` or `POST`.
    pub method: String,
    /// The header the account's key is sent in.
    pub api_key_header: String,
    /// What precedes the key in that header.
    pub api_key_prefix: String,
    /// Whether a sanitized mark was installed for this definition.
    pub has_mark: bool,
}
```

and `pub plugin: Option<PluginInfo>` on `ProviderDefinition`, defaulting to `None` at every existing construction site.

- [ ] **Step 4: Extend the registry**

Add to `crates/tidemarkd/src/registry.rs`:

- `builtin_ids()`, collecting `OAUTH`'s slugs, `keyed::CATALOG`'s ids and `HAND_WRITTEN`'s ids. This is the `reserved` argument every `schema::parse` call takes, so a plugin can never claim an id a future release ships.
- `catalog_with_plugins(config, plugins)` = the existing `catalog(config)`, then one `ProviderDefinition` per installed definition with `credential: CredentialKind::Key`, `credential_hint` naming the plugin's own service, `options: Vec::new()` (a plugin's configuration is its endpoint, not a `ProviderOption`), and `plugin: Some(PluginInfo { .. })`.
- `plugin_account(definition, account, secrets, config)`, building an `Account::new` whose `Factory` closure reads `config.plugin_endpoint(id, account)` and constructs a `PluginProvider`; when there is no endpoint the closure returns `ProviderError::Local("this account has no endpoint yet; set the metrics URL for it")`, which the engine already publishes as a message under a failure state.
- `accounts_with_plugins(secrets, config, plugins)`, the existing `accounts` with the plugin branch added for a configured provider that no compiled entry claims.
- `plugin_title(provider, plugins)`, consulted by `title` so a notification says `Acme AI`.

Keep `catalog` and `accounts` as thin wrappers that pass an empty plugin slice, so nothing outside the daemon changes shape.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p tidemarkd registry`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/tidemarkd/src/registry.rs crates/tidemark-types/src/wire.rs
git commit -m "feat(daemon): register installed plugin definitions"
```

---

### Task 14: Engine commands, D-Bus methods and the proxy

**Files:**
- Modify: `crates/tidemarkd/src/engine.rs` (`Command` variants, handlers, `Store` ownership)
- Modify: `crates/tidemarkd/src/service.rs` (five methods, one signal)
- Modify: `crates/tidemark-ipc/src/lib.rs`
- Modify: `crates/tidemarkd/src/main.rs` (open the store at startup)
- Test: colocated in `engine.rs` and `service.rs`

**Interfaces:**
- Consumes: Tasks 10–13.
- Produces on the interface `io.github.zbndev.Tidemark.Daemon1`:
  - `InspectPlugin(bytes: Vec<u8>) -> PluginInfo` — validate without storing
  - `InstallPlugin(bytes: Vec<u8>) -> PluginInfo` — validate, store, register
  - `RemovePlugin(provider: &str)` — refused while accounts remain
  - `SetPluginEndpoint(provider: &str, account: &str, endpoint: &str, allow_insecure_http: bool)`
  - `RenderPlugin(bytes: Vec<u8>, response: Vec<u8>) -> Presentation` — the pure authoring path
  - `PluginsChanged` signal carrying `Vec<PluginInfo>`
- And in `engine.rs`: `Command::{InspectPlugin, InstallPlugin, RemovePlugin, SetPluginEndpoint, RenderPlugin}`, each with a `oneshot::Sender` reply, exactly like `SetOption`.

- [ ] **Step 1: Write the failing engine tests**

```rust
    #[tokio::test]
    async fn installing_a_definition_makes_its_provider_configurable() {
        let mut engine = engine_with_no_accounts().await;
        let info = engine.install_plugin(FILE.as_bytes().to_vec()).await.expect("installs");
        assert_eq!(info.id, "com.acme.quota");
        assert!(engine.catalog().iter().any(|d| d.provider == "com.acme.quota"));
    }

    #[tokio::test]
    async fn an_endpoint_is_stored_per_account_and_rebuilds_only_that_client() {
        let mut engine = engine_with_plugin_accounts(&["default", "work"]).await;
        engine
            .set_plugin_endpoint("com.acme.quota", "work", "https://corp.test/u", false)
            .await
            .expect("stores");
        let config = engine.config_snapshot();
        assert!(config.plugin_endpoint("com.acme.quota", "work").unwrap().is_some());
        assert!(
            config.plugin_endpoint("com.acme.quota", "default").unwrap().is_none(),
            "one account's endpoint is not the other's"
        );
    }

    #[tokio::test]
    async fn a_plain_http_endpoint_without_the_acknowledgement_is_refused_by_the_daemon() {
        let mut engine = engine_with_plugin_accounts(&["default"]).await;
        let error = engine
            .set_plugin_endpoint("com.acme.quota", "default", "http://corp.test/u", false)
            .await
            .expect_err("the daemon refuses before storing");
        assert!(error.contains("http"), "{error}");
        assert!(
            engine.config_snapshot().plugin_endpoint("com.acme.quota", "default").unwrap().is_none()
        );
    }

    #[tokio::test]
    async fn removing_a_definition_is_refused_while_an_account_uses_it() {
        let mut engine = engine_with_plugin_accounts(&["default"]).await;
        assert!(engine.remove_plugin("com.acme.quota").await.is_err());
        engine.remove_provider("com.acme.quota", "default").await.expect("removes the account");
        engine.remove_plugin("com.acme.quota").await.expect("now removable");
    }

    #[tokio::test]
    async fn removing_an_account_removes_its_endpoint_with_its_credential() {
        let mut engine = engine_with_plugin_accounts(&["default", "work"]).await;
        engine
            .set_plugin_endpoint("com.acme.quota", "work", "https://corp.test/u", false)
            .await
            .expect("stores");
        engine.remove_provider("com.acme.quota", "work").await.expect("removes");
        assert!(
            engine.config_snapshot().plugin_endpoint("com.acme.quota", "work").unwrap().is_none(),
            "an endpoint is account state and goes with the account"
        );
    }

    #[tokio::test]
    async fn rendering_a_fixture_reaches_no_network_and_reads_no_key() {
        let mut engine = engine_with_no_accounts().await;
        let presentation = engine
            .render_plugin(FILE.as_bytes().to_vec(), br#"{"used": 1, "limit": 4}"#.to_vec())
            .await
            .expect("renders");
        assert_eq!(presentation.card.len(), 1);
    }
```

And in `service.rs`'s tests. Add one harness beside the module's existing `daemon_over`,
`daemon_over_plugins() -> (Daemon, DaemonProxy, PathBuf)`, which builds a daemon whose plugin
store roots at a scratch directory and hands back that root so a test can assert nothing was
written; `FILE` is Task 12's fixture constant:

```rust
    #[tokio::test]
    async fn inspecting_a_plugin_over_the_bus_stores_nothing() {
        let (daemon, proxy, plugins_root) = daemon_over_plugins().await;
        let info = proxy
            .inspect_plugin(FILE.as_bytes().to_vec())
            .await
            .expect("a valid file inspects");
        assert_eq!(info.id, "com.acme.quota");
        assert_eq!(info.api_key_header, "X-Acme-Key");
        assert_eq!(
            std::fs::read_dir(&plugins_root).map(|entries| entries.count()).unwrap_or(0),
            0,
            "inspection is a dry run"
        );
        assert!(
            proxy.list_providers().await.expect("catalog").iter().all(|d| d.plugin.is_none()),
            "and nothing became configurable"
        );
        drop(daemon);
    }

    #[tokio::test]
    async fn installing_an_invalid_plugin_is_an_invalid_argument_with_the_stage_named() {
        let (daemon, proxy, _root) = daemon_over_plugins().await;
        let broken = FILE.replace("language = \"lua54\"", "language = \"lua53\"");
        let error = proxy
            .install_plugin(broken.into_bytes())
            .await
            .expect_err("an unknown parser language is refused");
        let message = error.to_string();
        assert!(message.contains("parser.language"), "the stage is named: {message}");
        drop(daemon);
    }

    #[tokio::test]
    async fn setting_an_endpoint_for_an_unconfigured_account_is_an_invalid_argument() {
        let (daemon, proxy, _root) = daemon_over_plugins().await;
        proxy.install_plugin(FILE.as_bytes().to_vec()).await.expect("installs");
        let error = proxy
            .set_plugin_endpoint("com.acme.quota", "work", "https://a.test/u", false)
            .await
            .expect_err("there is no such account");
        assert!(matches!(error, zbus::Error::MethodError(..)), "{error:?}");
        drop(daemon);
    }

    #[tokio::test]
    async fn installing_a_plugin_emits_plugins_changed() {
        let (daemon, proxy, _root) = daemon_over_plugins().await;
        let mut changes = proxy.receive_plugins_changed().await.expect("subscribes");
        proxy.install_plugin(FILE.as_bytes().to_vec()).await.expect("installs");
        let signal = changes.next().await.expect("a signal arrives");
        let plugins = signal.args().expect("args").plugins;
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "com.acme.quota");
        drop(daemon);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemarkd plugin`
Expected: FAIL — `no method named install_plugin`.

- [ ] **Step 3: Add the commands and handlers**

In `engine.rs`: give `Engine` a `plugins: plugins::Store`; add the five `Command` variants with `oneshot` replies, dispatched in `run` exactly like `SetOption`; implement `install_plugin`, `remove_plugin`, `set_plugin_endpoint`, `inspect_plugin` and `render_plugin` as `pub async fn`s on `Engine`, all of which:

- validate through core (`schema::parse` with `registry::builtin_ids()`, `PluginProvider::new` for an endpoint — so a refused endpoint is refused by the one function that will actually use it),
- persist through `Config` or `plugins::Store`,
- then rebuild the affected client and reconcile the published topology, in that order.

`set_plugin_endpoint` validates by constructing an `Endpoint` and calling `PluginProvider::new` with the account's stored key — or with a placeholder `Credential` when no key is stored yet, since the URL rules do not depend on the key. Do not store an endpoint that construction refused.

Extend `remove_provider`'s existing account cleanup with `Config::remove_plugin_endpoint`.

In `service.rs`: five `async fn`s on the interface, each taking the provider mutation lock where it touches an account, calling through `config_request`, and mapping `PluginError`/`StoreError` to `fdo::Error::InvalidArgs` with the stage-naming message. Add the `plugins_changed` signal and emit it after a successful install or removal.

In `main.rs`: open the store with `paths::plugins_dir()` before the engine is built, pass it in, and pass its `installed()` to `registry::accounts_with_plugins`.

- [ ] **Step 4: Mirror the contract in the proxy**

In `crates/tidemark-ipc/src/lib.rs`, extend the `use tidemark_types::{...}` list with `PluginInfo`
and `Presentation`, then add the five methods and the signal with doc comments in the file's
existing voice:

```rust
    /// Validates a plugin file without storing it: what the import preview shows.
    fn inspect_plugin(&self, bytes: Vec<u8>) -> zbus::Result<PluginInfo>;

    /// Validates and installs a plugin file, replacing an earlier version of the same id.
    fn install_plugin(&self, bytes: Vec<u8>) -> zbus::Result<PluginInfo>;

    /// Removes an installed definition. Refused while any account still uses it.
    fn remove_plugin(&self, provider: &str) -> zbus::Result<()>;

    /// Sets one plugin account's complete metrics URL, and whether plain http is accepted
    /// for it. Per account, never shared: two accounts of one plugin are two endpoints.
    fn set_plugin_endpoint(
        &self,
        provider: &str,
        account: &str,
        endpoint: &str,
        allow_insecure_http: bool,
    ) -> zbus::Result<()>;

    /// Runs a plugin's parser against a local response fixture: the authoring loop, with no
    /// key read and no request made.
    fn render_plugin(&self, bytes: Vec<u8>, response: Vec<u8>) -> zbus::Result<Presentation>;

    /// The installed plugin definitions changed.
    #[zbus(signal)]
    fn plugins_changed(&self, plugins: Vec<PluginInfo>) -> zbus::Result<()>;
```

- [ ] **Step 5: Run the tests and probe the live bus**

```bash
cargo test -p tidemarkd -p tidemark-ipc
systemctl --user stop tidemarkd; cargo run -p tidemarkd &
busctl --user introspect io.github.zbndev.Tidemark.Daemon /io/github/zbndev/Tidemark | grep -i plugin
```

Expected: the five methods and the signal appear in the introspection output.

- [ ] **Step 6: Commit**

```bash
git add crates/tidemarkd crates/tidemark-ipc
git commit -m "feat(daemon): plugin import, endpoint and render over D-Bus"
```

---

### Task 15: Keep the key out of the recorded response

**Files:**
- Modify: `crates/tidemark-core/src/debug.rs`
- Test: colocated

**Interfaces:**
- Consumes: `debug::Exchange`, `debug::Answer`.
- Produces: `debug::set_account_secret(provider: &str, secret: Option<&str>)` and body redaction of an exact secret occurrence before persistence.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn a_response_that_echoes_the_accounts_key_is_recorded_without_it() {
        set_account_secret("com.acme.quota", Some("sk-live-abc123"));
        let line = recorded_line(Exchange {
            provider: "com.acme.quota",
            sent: sent("https://a.test/u"),
            answer: Answer::Body { status: 200, body: r#"{"echo":"sk-live-abc123","used":1}"# },
        });
        assert!(!line.contains("sk-live-abc123"), "{line}");
        assert!(line.contains("REDACTED"));
        assert!(line.contains("\"used\":1"), "the rest of the body is still worth reading");
    }

    #[test]
    fn a_short_or_absent_secret_is_never_used_as_a_needle() {
        // Redacting a two-character secret would blank half of every body and teach nobody
        // anything. A secret that short is a configuration mistake, not a redaction case.
        set_account_secret("com.acme.quota", Some("ab"));
        let line = recorded_line(Exchange {
            provider: "com.acme.quota",
            sent: sent("https://a.test/u"),
            answer: Answer::Body { status: 200, body: r#"{"about":"abacus"}"# },
        });
        assert!(line.contains("abacus"));
        set_account_secret("com.acme.quota", None);
    }

    #[test]
    fn one_providers_secret_is_not_looked_for_in_another_providers_body() {
        set_account_secret("com.acme.quota", Some("sk-live-abc123"));
        let line = recorded_line(Exchange {
            provider: "zai",
            sent: sent("https://zai.test/u"),
            answer: Answer::Body { status: 200, body: "sk-live-abc123" },
        });
        assert!(line.contains("sk-live-abc123"), "the needle belongs to one provider only");
        set_account_secret("com.acme.quota", None);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemark-core debug`
Expected: FAIL — `cannot find function set_account_secret`.

- [ ] **Step 3: Implement it**

Add a process-wide `RwLock<BTreeMap<String, String>>` of provider → secret beside the existing recorder state, set by the daemon when it builds a plugin client and cleared when it drops one; in `record`, before writing an `Answer::Body`, replace every exact occurrence of that provider's secret with `<REDACTED>`, skipping secrets shorter than 8 bytes. Document the reasoning in the module docs:

```rust
//! ... The recorder already keeps credentials out of what Tidemark *sends*. A plugin adds the
//! other direction: the endpoint is a stranger's, and one that echoes its own credential back
//! in a response would otherwise put it in this file. So the account's exact secret is a
//! needle, matched per provider and never across providers, and replaced before anything is
//! persisted.
```

- [ ] **Step 4: Call it from the daemon**

In `registry::plugin_account`'s factory closure, call `debug::set_account_secret(&definition.id, Some(credential.expose()))` immediately after reading the credential, and `None` when a plugin account is removed.

- [ ] **Step 5: Run the tests and commit**

```bash
cargo test -p tidemark-core debug && cargo test -p tidemarkd
git add crates/tidemark-core/src/debug.rs crates/tidemarkd/src/registry.rs
git commit -m "fix(core): redact an echoed key from a recorded plugin response"
```

---

### Task 16: `tidemarkctl plugin`

**Files:**
- Create: `crates/tidemark-cli/src/commands/plugin.rs`
- Modify: `crates/tidemark-cli/src/cli.rs`, `crates/tidemark-cli/src/commands/mod.rs`, `crates/tidemark-cli/src/main.rs`
- Test: colocated, plus `crates/tidemark-cli/tests/managing_a_fake_daemon.rs`

**Interfaces:**
- Consumes: Task 14's proxy methods; `PluginInfo`, `Presentation`.
- Produces the grammar:
  - `tidemarkctl plugin validate FILE`
  - `tidemarkctl plugin render FILE --response RESPONSE.json`
  - `tidemarkctl plugin install FILE`
  - `tidemarkctl plugin list`
  - `tidemarkctl plugin remove PROVIDER`
  - `tidemarkctl plugin endpoint PROVIDER URL [--account ACCOUNT] [--allow-insecure-http]`
  - All of them accept `--format json` where they print a reading or a listing.

- [ ] **Step 1: Write the failing tests**

In `crates/tidemark-cli/tests/managing_a_fake_daemon.rs`, using the existing fake-daemon harness in `tests/fake`:

```rust
#[test]
fn validating_a_plugin_sends_the_files_bytes_and_prints_what_it_declared() {
    let daemon = FakeDaemon::start();
    let file = write_temp("acme.tidemark-provider", MINIMAL_PLUGIN);
    let output = tidemarkctl(&daemon, &["plugin", "validate", file.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("com.acme.quota"));
    assert!(text.contains("Acme AI"));
    assert!(text.contains("X-Acme-Key"), "the header the key goes in is the point of the preview");
    assert_eq!(daemon.calls(), ["InspectPlugin"], "validation never installs");
}

#[test]
fn a_rejected_plugin_exits_nonzero_and_names_the_stage() {
    let daemon = FakeDaemon::refusing("InspectPlugin", "parser.language: must be lua54");
    let file = write_temp("bad.tidemark-provider", MINIMAL_PLUGIN);
    let output = tidemarkctl(&daemon, &["plugin", "validate", file.to_str().unwrap()]);
    assert_ne!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stderr).contains("parser.language"));
}

#[test]
fn rendering_reads_both_local_files_and_prints_the_resulting_layout() {
    let daemon = FakeDaemon::start();
    let file = write_temp("acme.tidemark-provider", MINIMAL_PLUGIN);
    let response = write_temp("acme.json", r#"{"used": 1, "limit": 4}"#);
    let output = tidemarkctl(
        &daemon,
        &["plugin", "render", file.to_str().unwrap(), "--response", response.to_str().unwrap(), "--format", "json"],
    );
    assert_eq!(output.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(json["card"].is_array());
    assert_eq!(daemon.calls(), ["RenderPlugin"]);
}

#[test]
fn a_missing_file_is_a_local_error_before_the_daemon_is_contacted() {
    let daemon = FakeDaemon::start();
    let output = tidemarkctl(&daemon, &["plugin", "validate", "/no/such/file"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(daemon.calls().is_empty(), "a file the CLI cannot read is not the daemon's problem");
}

#[test]
fn setting_an_endpoint_defaults_to_the_default_account_and_refuses_insecure_by_default() {
    let daemon = FakeDaemon::start();
    tidemarkctl(&daemon, &["plugin", "endpoint", "com.acme.quota", "https://a.test/u"]);
    assert_eq!(
        daemon.last_args("SetPluginEndpoint"),
        vec!["com.acme.quota".to_owned(), "default".to_owned(), "https://a.test/u".to_owned(), "false".to_owned()]
    );
    tidemarkctl(
        &daemon,
        &["plugin", "endpoint", "com.acme.quota", "http://a.test/u", "--account", "work", "--allow-insecure-http"],
    );
    assert_eq!(
        daemon.last_args("SetPluginEndpoint"),
        vec!["com.acme.quota".to_owned(), "work".to_owned(), "http://a.test/u".to_owned(), "true".to_owned()]
    );
}

#[test]
fn listing_and_removing_go_through_the_daemon() {
    let daemon = FakeDaemon::start();
    let output = tidemarkctl(&daemon, &["plugin", "list"]);
    assert_eq!(output.status.code(), Some(0));
    tidemarkctl(&daemon, &["plugin", "remove", "com.acme.quota"]);
    assert_eq!(daemon.last_args("RemovePlugin"), vec!["com.acme.quota".to_owned()]);
}
```

Add a colocated test in `cli.rs` for the grammar itself:

```rust
    #[test]
    fn the_plugin_subcommands_parse_the_way_help_says_they_do() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["tidemarkctl", "plugin", "validate", "a.tidemark-provider"]).is_ok());
        assert!(Cli::try_parse_from(["tidemarkctl", "plugin", "render", "a.tidemark-provider"]).is_err(),
            "render needs a fixture: it has no key and no network, so a response is the input");
        assert!(Cli::try_parse_from(["tidemarkctl", "plugin", "endpoint", "com.acme.q"]).is_err());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemark-cli plugin`
Expected: FAIL — `unrecognized subcommand 'plugin'`.

- [ ] **Step 3: Add the grammar**

In `crates/tidemark-cli/src/cli.rs`, add to `Command`:

```rust
    /// User-installed providers: validate, render, install, list and configure.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
```

and:

```rust
#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Check a plugin file and print what it declares. Nothing is installed and nothing is
    /// polled, so this is safe to run on a file somebody sent you.
    Validate {
        /// Path to a `.tidemark-provider` file.
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Run a plugin's parser against a saved JSON response. The authoring loop: no key is
    /// read and no request is made.
    Render {
        /// Path to a `.tidemark-provider` file.
        file: PathBuf,
        /// Path to a JSON response to transform.
        #[arg(long)]
        response: PathBuf,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Install a plugin file, replacing an earlier version of the same provider id.
    Install {
        /// Path to a `.tidemark-provider` file.
        file: PathBuf,
    },
    /// Every installed plugin definition.
    List {
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Remove an installed definition. Refused while any account still uses it.
    Remove {
        /// The plugin's provider id.
        provider: String,
    },
    /// The complete metrics URL one account sends its key to.
    Endpoint {
        /// The plugin's provider id.
        provider: String,
        /// The absolute URL. https unless insecure transport is confirmed.
        url: String,
        /// Another account id; defaults to `default`.
        #[arg(long, default_value = "default")]
        account: String,
        /// Accept plain http for this account, sending the key in clear.
        #[arg(long)]
        allow_insecure_http: bool,
    },
}
```

- [ ] **Step 4: Implement the commands**

Create `crates/tidemark-cli/src/commands/plugin.rs`:

```rust
//! `tidemarkctl plugin`: the authoring and administration surface for user-installed providers.
//!
//! Everything here reads local files and formats what comes back. It does not parse a plugin,
//! run Lua or sanitize an SVG — there is one parser and one runtime, both in the daemon, and a
//! second copy in the CLI would be a second set of verdicts to keep in step. That is also why
//! `render` sends both files over the bus rather than transforming anything locally.
```

with one function per subcommand: read the file (`std::fs::read`, exiting with the CLI's existing local-error path when it cannot), call the proxy method, and print through `format::text`/`format::json`. `validate` and `install` print id, name, plugin version, method, key header and prefix, whether a mark was accepted, and any diagnostics. `render` prints the metrics with their widget placements in the order the daemon returned, so a plugin author can see the card order without opening the GUI.

- [ ] **Step 5: Run the tests and check `--help` reads well**

```bash
cargo test -p tidemark-cli
cargo run -p tidemark-cli -- plugin --help
cargo run -p tidemark-cli -- plugin render --help
```

Expected: tests pass, and the help text names the file arguments and says the render path needs no key.

- [ ] **Step 6: Regenerate completions and commit**

```bash
cargo test -p tidemark-cli --test completions
git add crates/tidemark-cli
git commit -m "feat(cli): tidemarkctl plugin validate, render, install and endpoint"
```

---

### Task 17: The GUI import and account flow

**Files:**
- Create: `crates/tidemark/src/provider_settings/plugins.rs`
- Modify: `crates/tidemark/src/provider_settings/` (the module that lists providers and adds accounts)
- Modify: `crates/tidemark/src/bus.rs` (`Update::PluginsChanged`)
- Modify: `crates/tidemark/src/mark.rs` (add the plugin icon search path)
- Modify: `crates/tidemark/src/main.rs` (register that path once at startup)
- Test: colocated, pure functions only

**Interfaces:**
- Consumes: `PluginInfo`, `ProviderDefinition::plugin`, `paths`-equivalent path published by the daemon (see below), `DaemonProxy::{inspect_plugin, install_plugin, remove_plugin, set_plugin_endpoint}`.
- Produces:
  - `plugins::preview(info: &PluginInfo) -> Vec<(String, String)>` — the label/value rows the import dialog shows
  - `plugins::endpoint_warning(url: &str) -> Option<String>` — the sentence shown before a plain-http endpoint is accepted
  - `plugins::valid_endpoint(url: &str) -> bool` — whether the confirm button is enabled
  - `mark::add_plugin_path(display: &gdk::Display, root: &Path)`

The GUI has no `paths` of its own (that is core, which it may not depend on), so the daemon publishes the icon root: add `plugin_icons_path: String` to `DataInfo` in `wire.rs`, filled from `paths::plugin_icons_dir()`, and the GUI reads it from `get_data_info` at startup. Note it in the `DataInfo` doc comment as what it is — a path the GUI adds to its icon search path, not a path it reads files from itself.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn the_preview_shows_the_two_facts_a_user_must_check_before_the_first_request() {
        let rows = preview(&info("Authorization", "Bearer ", "POST"));
        let flattened: Vec<(&str, &str)> =
            rows.iter().map(|(l, v)| (l.as_str(), v.as_str())).collect();
        assert!(flattened.contains(&("Key header", "Authorization")));
        assert!(flattened.contains(&("Key prefix", "Bearer ")));
        assert!(flattened.contains(&("Request", "POST")));
        assert!(flattened.contains(&("Provider id", "com.acme.quota")));
        assert!(flattened.contains(&("Plugin version", "1.0.0")));
    }

    #[test]
    fn an_empty_prefix_is_shown_as_the_absence_it_is_rather_than_left_out() {
        let rows = preview(&info("X-Acme-Key", "", "GET"));
        let prefix = rows.iter().find(|(label, _)| label == "Key prefix").expect("shown");
        assert_eq!(prefix.1, "none", "a missing prefix is a fact about the request, not a blank");
    }

    #[test]
    fn a_plain_http_endpoint_warns_about_the_key_before_it_is_accepted() {
        let warning = endpoint_warning("http://metrics.corp.test/u").expect("warns");
        assert!(warning.contains("key"), "{warning}");
        assert_eq!(endpoint_warning("https://metrics.corp.test/u"), None);
    }

    #[test]
    fn the_confirm_button_is_only_enabled_for_an_endpoint_the_daemon_would_accept() {
        assert!(valid_endpoint("https://metrics.corp.test/v1/usage?window=month"));
        assert!(valid_endpoint("http://metrics.corp.test/u"));
        for bad in ["", "  ", "metrics.corp.test/u", "/v1/usage", "ftp://a.test/u",
                    "https://user:pw@a.test/u", "https://a.test/u#frag"] {
            assert!(!valid_endpoint(bad), "{bad} would be refused, so do not offer to send it");
        }
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tidemark plugins`
Expected: FAIL — module not found.

- [ ] **Step 3: Write the pure helpers, then the dialogs**

Create `crates/tidemark/src/provider_settings/plugins.rs` with the three pure functions above and the GTK flow built on them:

- An **Import** button on the provider settings page opens a `gtk::FileDialog` filtered to `*.tidemark-provider`, reads the bytes, calls `inspect_plugin`, and shows an `adw::AlertDialog` (or a page, matching the surrounding style) with `preview`'s rows, the mark rendered from the icon theme when `has_mark`, and any diagnostic message from a refusal. Installing is a second, explicit action on that dialog and calls `install_plugin`.
- **Adding an account** for a provider whose definition carries `plugin: Some(_)` asks for the account name, the complete endpoint URL and the API key in one form, shows the final URL, method and key-header shape together, disables confirm until `valid_endpoint`, and shows `endpoint_warning` with an explicit "send the key over plain http" switch when the URL is `http://`. On confirm: `add_account`, then `set_plugin_endpoint`, then `set_key` — in that order, so a key is never stored for an account with no endpoint.
- **Removing** an installed definition is offered only when no account uses it, and says which accounts to remove first when they do (the daemon's refusal message is the text).

Keep every one of those bodies free of policy: the daemon refuses what it refuses, and the dialog shows the message.

- [ ] **Step 4: Make plugin marks load through the icon theme**

In `mark.rs`:

```rust
/// Adds a directory of installed plugin marks to this display's icon theme.
///
/// Called once, at startup, with the root the daemon publishes in [`DataInfo`]. After this a
/// plugin mark is found by [`icon_name`] exactly the way a shipped mark is — which is the
/// whole reason the daemon materializes it as a file rather than sending bytes: GTK only
/// recolours a symbolic SVG it loaded *through the icon theme*, and a texture built from
/// bytes would be a black smudge on a dark theme. See this module's own note on why.
pub fn add_plugin_path(display: &gtk::gdk::Display, root: &std::path::Path) {
    gtk::IconTheme::for_display(display).add_search_path(root);
}
```

and call it from `main.rs` once the first `get_data_info` reply arrives.

- [ ] **Step 5: Run the tests and look at the real window**

```bash
cargo test -p tidemark
systemctl --user stop tidemarkd; cargo run -p tidemarkd &
xvfb-run -a --server-args='-screen 0 1280x900x24' sh -c \
  'cargo run -p tidemark & sleep 12; xdotool search --name Tidemark; import -window root /tmp/claude-1000/import.png'
```

Expected: the Import button is on the provider settings page, and the preview dialog shows the id, name, version, method, key header and prefix. Check the screenshot rather than reasoning about the layout.

- [ ] **Step 6: Commit**

```bash
git add crates/tidemark crates/tidemark-types/src/wire.rs crates/tidemarkd/src/service.rs
git commit -m "feat(ui): import plugin providers and configure their endpoints"
```

---

### Task 18: HTTP integration coverage

**Files:**
- Create: `crates/tidemark-core/tests/plugin_provider.rs`
- Create: `crates/tidemark-core/tests/fixtures/plugin/acme.tidemark-provider`
- Create: `crates/tidemark-core/tests/fixtures/plugin/acme-response.json`
- Test: the file is the test

**Interfaces:**
- Consumes: Task 10's `PluginProvider`/`Endpoint`/`render`, Task 5's `schema::parse`.
- Produces: no new API. The fixture pair is also what the author guide and Task 20's example reference, so it is written once here and pointed at from there.

- [ ] **Step 1: Write the fixtures**

`acme.tidemark-provider` is the spec's own example, reduced to what a test needs and carrying a mark; `acme-response.json` is a **synthetic** nested response with the shapes the corpus showed — nested objects, an array of models, a numeric string, a `null` limit, a `remaining` to subtract and an RFC 3339 reset. No real endpoint, no real key, no recorded live body: the guide's own rule, and the repository's.

- [ ] **Step 2: Write the harness**

The bodies below all lean on one recording server and three constructors. Model the server on
whatever `crates/tidemark-core/tests/proxy.rs` already uses rather than adding a dev-dependency:

```rust
/// A test key. Long enough that the debug recorder's own needle rule applies to it, and
/// obviously not a real one.
const KEY: &str = "sk-test-not-a-real-key";
/// The fixture body, so a test that only needs a reading does not read a file.
const RESPONSE: &str = include_str!("fixtures/plugin/acme-response.json");
/// A parser that reports what it can see, for the credential-leak assertion.
const REFLECTING_PARSER: &str = r#"
function parse(response, context)
    local metrics = {}
    for index, key in ipairs(sorted_keys(response)) do
        metrics[index] = { id = "k" .. index, title = key, text = tostring(response[key]) }
    end
    return { metrics = metrics, card = {}, details = {} }
end
"#;

/// A server that records every request and answers with a scripted `Reply`.
struct Recording { /* listener, recorded requests, the next reply */ }

/// One plugin account over `server`, with `method`, `header` and `prefix` declared.
async fn plugin_over(server: &Recording, method: &str, header: &str, prefix: &str) -> PluginProvider;
/// One plugin account over `server` with a parser of its own.
async fn plugin_over_source(server: &Recording, source: &str) -> PluginProvider;
/// One plugin account pointed at an absolute URL that is not this machine.
async fn plugin_at(url: &str) -> PluginProvider;
/// The presentation of one body, so a test can fill a `ProviderStatus` the way the daemon does.
fn plugin_presentation(provider: &PluginProvider, body: &str) -> Presentation;
```

- [ ] **Step 3: Write the failing tests**

```rust
//! One plugin account against a local server: the transport boundary, end to end.
//!
//! The unit tests prove the parser and the sandbox. This file proves the parts only a socket
//! can: that the declared header carries the key and nothing else does, that a redirect does
//! not move it, that an oversized body is refused before it is parsed, and that a failed poll
//! leaves the last good reading in place.

#[tokio::test]
async fn a_get_sends_the_declared_header_and_nothing_that_was_not_declared() {
    let server = Recording::start(Reply::json(RESPONSE)).await;
    let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;
    provider.fetch().await.expect("polls");

    let seen = server.last_request();
    assert_eq!(seen.method, "GET");
    assert_eq!(seen.header("x-acme-key").as_deref(), Some("sk-test-not-a-real-key"));
    assert_eq!(seen.header("accept").as_deref(), Some("application/json"));
    assert_eq!(seen.header("user-agent"), Some(tidemark_types::user_agent()));
    assert!(seen.header("cookie").is_none());
    assert!(seen.header("authorization").is_none(), "only the declared header carries the key");
}

#[tokio::test]
async fn an_empty_post_sends_no_body_and_no_content_type_a_plugin_chose() {
    let server = Recording::start(Reply::json(RESPONSE)).await;
    let provider = plugin_over(&server, "POST", "Authorization", "Bearer ").await;
    provider.fetch().await.expect("polls");

    let seen = server.last_request();
    assert_eq!(seen.method, "POST");
    assert!(seen.body.is_empty(), "a plugin has no way to send a body");
    assert_eq!(seen.header("content-length").as_deref(), Some("0"));
    assert_eq!(seen.header("authorization").as_deref(), Some("Bearer sk-test-not-a-real-key"));
}

#[tokio::test]
async fn a_redirect_to_another_origin_is_not_followed_and_the_key_does_not_move() {
    let elsewhere = Recording::start(Reply::json(RESPONSE)).await;
    let server = Recording::start(Reply::redirect(302, &elsewhere.url("/usage"))).await;
    let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;

    let error = provider.fetch().await.expect_err("a 302 is not a reading");
    assert!(matches!(error, ProviderError::Http { status: 302 }), "{error:?}");
    assert!(
        elsewhere.requests().is_empty(),
        "the secret header must not follow a redirect to another origin"
    );
}

#[tokio::test]
async fn a_body_larger_than_the_limit_is_refused_without_being_parsed() {
    let oversized = format!("{{\"pad\":\"{}\"}}", "x".repeat(limits::RESPONSE_BYTES));
    let server = Recording::start(Reply::json(&oversized)).await;
    let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;
    let error = provider.fetch().await.expect_err("the body is over the bound");
    assert!(matches!(error, ProviderError::Malformed(_)), "{error:?}");
    assert!(
        error.to_string().contains("response body"),
        "the message names the bound that was hit: {error}"
    );
}

#[tokio::test]
async fn each_status_maps_onto_the_state_the_engine_already_knows() {
    for (reply, expected) in [
        (Reply::status(401), "credential"),
        (Reply::status(403), "credential"),
        (Reply::rate_limited(90), "rate-limited"),
        (Reply::status(500), "http"),
        (Reply::json("{ not json"), "malformed"),
    ] {
        let server = Recording::start(reply).await;
        let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;
        let error = provider.fetch().await.expect_err("none of these is a reading");
        let actual = match &error {
            ProviderError::Credential { .. } => "credential",
            ProviderError::RateLimited { retry_after } => {
                assert_eq!(*retry_after, Some(90), "the wait the provider asked for is kept");
                "rate-limited"
            }
            ProviderError::Http { .. } => "http",
            ProviderError::Malformed(_) => "malformed",
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(actual, expected, "{error:?}");
    }
}

#[tokio::test]
async fn the_lua_input_never_contains_the_credential() {
    // The parser here reports every top-level key it can see, so if the credential or a
    // request header had been handed to Lua it would appear in the metric titles.
    let server = Recording::start(Reply::json(RESPONSE)).await;
    let provider = plugin_over_source(&server, REFLECTING_PARSER).await;
    let snapshot = provider.fetch().await.expect("polls");
    let rendered = format!("{snapshot:?}");
    assert!(!rendered.contains(KEY), "the key reached the sandbox: {rendered}");
    assert!(!rendered.to_lowercase().contains("x-acme-key"));
}

#[tokio::test]
async fn a_malformed_response_leaves_the_previous_reading_in_place() {
    // Two polls of one account, the way `provider_to_history.rs` drives a built-in provider:
    // the first is a reading, the second is nonsense, and the card must still show the first.
    let server = Recording::start(Reply::json(RESPONSE)).await;
    let mut history = History::in_memory().expect("in-memory history");
    let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;

    let first = provider.fetch().await.expect("a reading");
    history.ingest(&first).expect("ingests");
    let mut status = ProviderStatus::pending(&first.provider, &first.account);
    status.set_reading(&first, plugin_presentation(&provider, RESPONSE));

    server.answer_next(Reply::json("{ not json"));
    let error = provider.fetch().await.expect_err("nonsense");
    status.set_state(ProviderState::Malformed, Some(error.to_string()));

    assert_eq!(status.windows.len(), 1, "the last good windows survive the failure");
    assert!(status.presentation.is_some(), "and so does its layout");
    assert_eq!(
        history.current_points(&first.provider, &first.account, first.windows[0].key.as_str())
            .expect("readable")
            .len(),
        1,
        "and no history point was fabricated from the failure"
    );
}

#[tokio::test]
async fn the_published_wire_form_carries_no_credential() {
    let server = Recording::start(Reply::json(RESPONSE)).await;
    let provider = plugin_over(&server, "GET", "X-Acme-Key", "").await;
    let snapshot = provider.fetch().await.expect("a reading");
    let mut status = ProviderStatus::pending(&snapshot.provider, &snapshot.account);
    status.set_reading(&snapshot, plugin_presentation(&provider, RESPONSE));

    let encoded = zvariant::to_bytes(zvariant::serialized::Context::new_dbus(zvariant::LE, 0), &status)
        .expect("the published shape encodes");
    let haystack = encoded.bytes();
    assert!(
        !haystack.windows(KEY.len()).any(|window| window == KEY.as_bytes()),
        "no credential may appear anywhere in what the daemon publishes"
    );
}

#[tokio::test]
async fn the_proxy_policy_and_certificate_validation_are_the_applications_own() {
    // The same two assertions `proxy.rs` already makes for built-in providers: a configured
    // proxy is used for a remote host, and loopback bypasses it — a plugin gets no say in
    // either, which is the point.
    let proxy = Recording::start(Reply::json(RESPONSE)).await;
    http::set_proxy(Proxy::new("http", &proxy.host(), proxy.port()).expect("valid").into());

    let server = Recording::start(Reply::json(RESPONSE)).await;
    let loopback = plugin_over(&server, "GET", "X-Acme-Key", "").await;
    loopback.fetch().await.expect("polls");
    assert!(proxy.requests().is_empty(), "loopback bypasses the shared proxy");

    let remote = plugin_at("https://metrics.example.test/usage").await;
    let _ = remote.fetch().await;
    assert!(!proxy.requests().is_empty(), "a remote host goes through it");

    http::set_proxy(None);
}
```

- [ ] **Step 4: Add the history and notification assertions**

The window a plugin declares must reach the existing history, pace and notification model, so
add two more to the same file:

```rust
#[tokio::test]
async fn a_plugin_window_accumulates_history_across_polls_like_any_other() {
    // Two readings with different consumption, one segment, two points — and the segment
    // identity is the plugin's own window key, so a renamed title does not split it.
}

#[tokio::test]
async fn a_plugin_window_can_be_opted_into_notifications_by_its_key() {
    // `Engine::set_window_notify` validates against the account's published windows, so this
    // proves a plugin window is published in the shape that check accepts.
}
```

Fill both by following `crates/tidemark-core/tests/provider_to_history.rs`, which already does
exactly this for a built-in provider — the point of the two tests is that nothing about the
plugin path needed a second mechanism.

- [ ] **Step 5: Run them**

Run: `cargo test -p tidemark-core --test plugin_provider`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/tidemark-core/tests
git commit -m "test(core): plugin transport, redaction and failure semantics"
```

---

### Task 19: Cross-platform CI and the size budget

**Files:**
- Create: `scripts/check-plugin-size.sh`
- Modify: `.github/workflows/` (the CI workflow that builds Linux and Windows)
- Test: the script is the test

**Interfaces:**
- Consumes: a release build of `tidemarkd`.
- Produces: `scripts/check-plugin-size.sh <baseline-commit>` — exits non-zero when the stripped Linux `tidemarkd` grew more than 1.5 MiB.

- [ ] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
# The plugin runtime's binary-size budget, from the design: vendored Lua and the plugin
# machinery may add at most 1.5 MiB to the stripped Linux daemon. Over budget, the dependency
# decision goes back to design review rather than being accepted quietly — so this is a gate,
# not a report.
set -euo pipefail
cd "$(dirname "$0")/.."

baseline_ref=${1:?usage: check-plugin-size.sh <baseline-commit>}
budget=$((1536 * 1024))
binary=target/release/tidemarkd

size_at() {
    # Built in a detached worktree so the working tree is never touched, and with the release
    # profile the packaging uses — a debug build's size says nothing about what ships.
    local ref=$1 worktree
    worktree=$(mktemp -d)
    git worktree add --detach --quiet "$worktree" "$ref"
    (cd "$worktree" && cargo build --release --locked -p tidemarkd >&2)
    stat -c %s "$worktree/$binary"
    git worktree remove --force "$worktree"
}

baseline=$(size_at "$baseline_ref")
cargo build --release --locked -p tidemarkd
current=$(stat -c %s "$binary")
delta=$((current - baseline))

printf 'baseline %s: %s bytes\ncurrent:  %s bytes\ndelta:    %s bytes (budget %s)\n' \
    "$baseline_ref" "$baseline" "$current" "$delta" "$budget"

if [ "$delta" -gt "$budget" ]; then
    printf 'over the plugin size budget: this returns to design review, per the spec\n' >&2
    exit 1
fi
echo 'plugin size ok'
```

- [ ] **Step 2: Run it against the pre-feature commit**

```bash
chmod +x scripts/check-plugin-size.sh
./scripts/check-plugin-size.sh 650f6d0
```
Expected: it prints the baseline size, the current size and the delta, and exits 0. **If it exits 1, stop and report the number** — per the spec that is a design decision, not something to work around.

- [ ] **Step 3: Extend CI**

In the workflow: add the vendored-Lua build prerequisites where they are missing (`mlua`'s vendored build needs a C compiler, which both runners already have); add a Windows MSYS2 UCRT64 job step that runs `cargo test -p tidemark-core plugin` so the same fixture executes on `stable-x86_64-pc-windows-gnu`; add a Linux step running `scripts/check-plugin-size.sh ${{ github.event.pull_request.base.sha || 'origin/main' }}`.

- [ ] **Step 4: Add the cross-platform determinism test**

In `crates/tidemark-core/tests/plugin_provider.rs`:

```rust
#[test]
fn the_same_fixture_produces_the_same_typed_result_on_every_platform() {
    // The whole reason `pairs` and `math.random` are gone: two platforms iterating one JSON
    // object in two orders would publish two card orders. So this asserts the exact ids, the
    // exact order and the exact numbers, and a platform that disagrees fails here rather than
    // on somebody's machine.
    let definition = schema::parse(
        include_bytes!("fixtures/plugin/acme.tidemark-provider"),
        &[],
    )
    .expect("the fixture parses");
    let reading = render(
        &definition,
        include_bytes!("fixtures/plugin/acme-response.json"),
        &AccountId::default(),
        Timestamp::from_unix(1_788_870_896).expect("plausible"),
    )
    .expect("renders");

    let ids: Vec<&str> = reading.presentation.metrics.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["month-total-cost", "month-sonnet-cost", "month-tokens"]);
    let card: Vec<(&str, &str)> = reading
        .presentation
        .card
        .iter()
        .map(|widget| (widget.kind.as_str(), widget.metric.as_str()))
        .collect();
    assert_eq!(
        card,
        [
            ("gauge", "month-total-cost"),
            ("value", "month-sonnet-cost"),
            ("ratio", "month-tokens"),
        ]
    );
    assert_eq!(reading.presentation.metric("month-total-cost").unwrap().used_percent, Some(25.0));
    assert_eq!(reading.snapshot.windows.len(), 1);
    assert_eq!(reading.snapshot.windows[0].key.as_str(), "monthly/w2592000");
}
```

- [ ] **Step 5: Commit**

```bash
git add scripts/check-plugin-size.sh .github/workflows crates/tidemark-core/tests
git commit -m "ci: run the plugin fixture on Windows and gate the size budget"
```

---

### Task 20: The plugin author guide

**Files:**
- Create: `docs/plugin-providers.md`
- Create: `examples/plugins/acme-quota.tidemark-provider`
- Create: `examples/plugins/acme-response.json`
- Modify: `README.md` (link the guide)
- Modify: `CONTEXT.md` (the ownership paragraph for the plugin path)
- Modify: `crates/tidemark-core/src/plugin/AGENTS.md` (create it: the crate convention is a domain AGENTS.md)

**Interfaces:**
- Consumes: every earlier task's real behaviour.
- Produces: documentation. The example plugin and fixture are the same files Task 18 tests, symlinked or copied so the guide's code is code that runs.

- [ ] **Step 1: Write the guide**

Ten sections, in the spec's order: a complete five-minute example; the TOML field reference; the JSON-to-Lua mapping and `null`; every available global and the list of what is absent and why; the metric schema and a gallery of `gauge`, `value`, `ratio` and `status`; recipes for conditional fields, loops, model lookup, numeric strings and `used = limit - remaining`; the SVG rules with a sanitizer error reference; the `tidemarkctl plugin validate` / `plugin render` loop; the security model and every limit from `limits.rs` verbatim; versioning, replacement and compatibility.

Two rules the guide must state and must itself obey: **never put a real endpoint, API key or recorded live response in a plugin or a fixture**, and **the account owner chooses the URL** — a plugin cannot name one, which is why a shared corporate definition is safe to circulate.

- [ ] **Step 2: Check the guide's example actually runs**

```bash
cargo run -p tidemark-cli -- plugin validate examples/plugins/acme-quota.tidemark-provider
cargo run -p tidemark-cli -- plugin render examples/plugins/acme-quota.tidemark-provider \
  --response examples/plugins/acme-response.json
```
Expected: both succeed, and the rendered card order matches what the guide's gallery section says it is. Fix the guide, not the output.

- [ ] **Step 3: Link it and record the ownership**

Add a README line under the existing provider documentation, and a `CONTEXT.md` paragraph saying which crate owns which half of the plugin path — the same split as this plan's File Structure, in `CONTEXT.md`'s voice.

- [ ] **Step 4: Verify every limit in the guide against the code**

```bash
grep -n 'pub const' crates/tidemark-core/src/plugin/limits.rs
grep -n 'KiB\|MiB\|1,000,000\|128\|32' docs/plugin-providers.md
```
Expected: every constant in `limits.rs` appears in the guide with the same number. A limit documented wrong is worse than one not documented.

- [ ] **Step 5: Commit**

```bash
git add docs examples README.md CONTEXT.md crates/tidemark-core/src/plugin/AGENTS.md
git commit -m "docs: plugin provider author guide and example"
```

---

## Final verification

Run the whole gate, then the spec's "actual surface" walkthrough — with a fake endpoint, not the test suite alone:

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && ./scripts/check-layering.sh
scripts/check-desktop-integration.sh
./scripts/check-plugin-size.sh 650f6d0
```

Then, by hand, in order:

1. import `examples/plugins/acme-quota.tidemark-provider` through the GUI;
2. add an account, entering a local fake endpoint and a test key;
3. fetch one response through `GET` and one through empty `POST`;
4. confirm the configured gauge/value/ratio order on the card;
5. open the details and confirm its independent order;
6. confirm the sanitized mark is drawn, and recoloured with the theme;
7. restart the daemon and the GUI, and confirm the definition, endpoint, account and key persisted;
8. make the fake endpoint answer malformed JSON, and confirm the last-good values stay visible under a failure chip;
9. remove the account, then uninstall the definition — and confirm the uninstall was refused until the account was gone.

Record what each step showed. A step that cannot be demonstrated is not done, whatever the tests say.
