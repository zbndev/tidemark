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
    let document = document(bytes)?;

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
    // Reservation is checked before shape, so a Tidemark-owned id is refused as reserved
    // rather than as malformed: a built-in slug like `zai` has no dot and would otherwise
    // fail the reverse-DNS rule first, telling the author the wrong thing.
    if id.starts_with("tidemark.") || id == "tidemark" || reserved.contains(&id.as_str()) {
        return Err(PluginError::ReservedId { id });
    }
    if !valid_id(&id) {
        return Err(PluginError::Schema {
            field: "provider.id",
            reason: "must be lowercase reverse-DNS with at least one dot, matching \
                     [a-z0-9]+(?:[.-][a-z0-9]+)*"
                .to_owned(),
        });
    }

    let name = string(&document, "provider", "name", "provider.name")?;
    bound("provider.name", name.len(), limits::MAX_LABEL_BYTES)?;
    let plugin_version = string(
        &document,
        "provider",
        "plugin_version",
        "provider.plugin_version",
    )?;
    bound(
        "provider.plugin_version",
        plugin_version.len(),
        limits::MAX_LABEL_BYTES,
    )?;

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

    let api_key_header = string(
        &document,
        "request",
        "api_key_header",
        "request.api_key_header",
    )?;
    if !header_name(&api_key_header) {
        return Err(PluginError::Schema {
            field: "request.api_key_header",
            reason: "must be an RFC 9110 field name: visible ASCII without separators".to_owned(),
        });
    }
    if FORBIDDEN_HEADERS.contains(&api_key_header.to_ascii_lowercase().as_str()) {
        return Err(PluginError::ForbiddenHeader {
            header: api_key_header,
        });
    }

    let api_key_prefix =
        optional_string(&document, "request", "api_key_prefix")?.unwrap_or_default();
    bound(
        "request.api_key_prefix",
        api_key_prefix.len(),
        limits::MAX_LABEL_BYTES,
    )?;
    if api_key_prefix
        .bytes()
        .any(|byte| !(0x20..=0x7e).contains(&byte))
    {
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
    bound(
        "the parser source",
        lua_source.len(),
        limits::LUA_SOURCE_BYTES,
    )?;
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
        // Only the rewritten form reaches the definition: what the author wrote stays in
        // `bytes`, and what Tidemark will ever draw is what the sanitizer produced.
        icon_svg: match icon {
            Some(source) => Some(super::svg::sanitize(&source)?),
            None => None,
        },
        bytes: bytes.to_vec(),
    })
}

/// The plugin file as TOML, or the one failure that says it is not a plugin file at all.
fn document(bytes: &[u8]) -> Result<DocumentMut, PluginError> {
    let text = std::str::from_utf8(bytes).map_err(|error| PluginError::Unreadable {
        path_hint: "the plugin file".to_owned(),
        reason: error.to_string(),
    })?;
    text.parse()
        .map_err(|error: toml_edit::TomlError| PluginError::Unreadable {
            path_hint: "the plugin file".to_owned(),
            reason: error.to_string(),
        })
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
        Some(item) => item
            .as_str()
            .map(|text| Some(text.to_owned()))
            .ok_or(PluginError::Schema {
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
        assert_eq!(
            definition.bytes,
            MINIMAL.as_bytes(),
            "the exact validated bytes are kept"
        );
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
            (
                "api_key_header = \"Authorization\"\n",
                "request.api_key_header",
            ),
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
        for id in [
            "acme",
            "Com.Acme",
            "com..acme",
            ".com.acme",
            "com.acme.",
            "com acme",
            "",
        ] {
            let text = MINIMAL.replace("com.acme.quota", id);
            assert!(parse_str(&text).is_err(), "id {id:?} must be refused");
        }
        assert!(valid_id("com.acme.quota"));
        assert!(valid_id("io.example.team-metrics"));
        assert!(
            !valid_id("acme"),
            "an id with no dot could collide with a built-in slug"
        );
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
            assert!(
                parse_str(&text).is_err(),
                "method {method} is outside the format"
            );
        }
    }

    #[test]
    fn a_header_that_could_move_the_request_or_the_secret_is_refused() {
        for header in [
            "Host",
            "Cookie",
            "Connection",
            "Content-Length",
            "Transfer-Encoding",
            "Proxy-Authorization",
            "Proxy-Connection",
            "TE",
            "Trailer",
            "Upgrade",
            "host",
            "COOKIE",
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
        for header in [
            "",
            "Auth orization",
            "Auth:orization",
            "Auth\nization",
            "Authörization",
        ] {
            let text = MINIMAL.replace("\"Authorization\"", &format!("{header:?}"));
            assert!(
                parse_str(&text).is_err(),
                "{header:?} is not an RFC header name"
            );
        }
    }

    #[test]
    fn a_missing_prefix_means_the_key_goes_in_bare() {
        let text = MINIMAL.replace("api_key_prefix = \"Bearer \"\n", "");
        assert_eq!(
            parse_str(&text)
                .expect("the prefix is optional")
                .api_key_prefix,
            ""
        );
    }

    #[test]
    fn an_unknown_parser_language_is_refused_rather_than_assumed_to_be_lua() {
        let text = MINIMAL.replace("\"lua54\"", "\"lua53\"");
        assert!(parse_str(&text).is_err());
    }

    #[test]
    fn a_source_without_a_parse_function_is_refused_at_the_schema_stage() {
        let text = MINIMAL.replace(
            "function parse(response, context)",
            "function transform(response)",
        );
        assert!(matches!(parse_str(&text), Err(PluginError::Schema { .. })));
    }

    #[test]
    fn the_file_and_source_size_limits_are_enforced() {
        let long = "-".repeat(limits::LUA_SOURCE_BYTES + 1);
        let text = MINIMAL.replace(
            "    return { metrics = {}, card = {}, details = {} }",
            &long,
        );
        assert!(matches!(
            parse_str(&text),
            Err(PluginError::TooLarge { .. })
        ));

        let huge = vec![b'x'; limits::PLUGIN_FILE_BYTES + 1];
        assert!(matches!(
            parse(&huge, &[]),
            Err(PluginError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_file_that_is_not_toml_or_not_utf8_is_a_schema_error_not_a_panic() {
        assert!(parse(b"\xff\xfe not utf8", &[]).is_err());
        assert!(parse(b"format_version = ", &[]).is_err());
    }

    #[test]
    fn a_name_longer_than_the_bound_is_refused() {
        let text = MINIMAL.replace("Acme AI", &"A".repeat(limits::MAX_LABEL_BYTES + 1));
        assert!(matches!(
            parse_str(&text),
            Err(PluginError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_mark_that_survives_sanitization_reaches_the_definition_canonically() {
        let text = format!(
            "{MINIMAL}\n[icon]\nsvg = \'\'\'\n<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 64 64\"><path fill=\"currentColor\" d=\"M0 0h1v1H0z\"/></svg>\n\'\'\'\n"
        );
        let icon = parse_str(&text)
            .expect("parses")
            .icon_svg
            .expect("a mark was declared");
        assert!(icon.contains("currentColor"));
    }

    #[test]
    fn a_hostile_mark_refuses_the_whole_file() {
        let text =
            format!("{MINIMAL}\n[icon]\nsvg = \'\'\'\n<svg><script>x()</script></svg>\n\'\'\'\n");
        assert!(matches!(parse_str(&text), Err(PluginError::Svg { .. })));
    }
}
