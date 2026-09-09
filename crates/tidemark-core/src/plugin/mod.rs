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

pub use crate::providers::Reading;

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
    /// D-Bus. Filled by `svg::sanitize` — see Task 6.
    pub icon_svg: Option<String>,
    /// The exact validated file bytes, stored unchanged so the definition stays
    /// inspectable and exportable.
    pub bytes: Vec<u8>,
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
