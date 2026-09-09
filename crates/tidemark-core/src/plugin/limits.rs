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
