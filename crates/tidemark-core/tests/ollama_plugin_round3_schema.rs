//! ROUND-3 — hostile mutations of the plugin FILE itself (schema stage).
//! None of these were covered by rounds 1-2: they attack the TOML contract,
//! the header rules, the id rules and the ignored-field surface.

use tidemark_core::plugin::schema;

const FILE: &str = include_str!("fixtures/plugin/ollama-cloud.tidemark-provider");

fn parse(
    text: &str,
) -> Result<tidemark_core::plugin::Definition, tidemark_core::plugin::PluginError> {
    schema::parse(text.as_bytes(), &[])
}

fn mutant(replace: &str, with: &str) -> String {
    assert!(FILE.contains(replace), "needle not found: {replace}");
    FILE.replacen(replace, with, 1)
}

#[test]
fn t1_lowercase_forbidden_header_is_refused() {
    assert!(matches!(
        parse(&mutant(
            "api_key_header = \"Authorization\"",
            "api_key_header = \"host\""
        )),
        Err(tidemark_core::plugin::PluginError::ForbiddenHeader { .. })
    ));
}

#[test]
fn t2_cookie_header_is_refused() {
    assert!(matches!(
        parse(&mutant(
            "api_key_header = \"Authorization\"",
            "api_key_header = \"Cookie\""
        )),
        Err(tidemark_core::plugin::PluginError::ForbiddenHeader { .. })
    ));
}

#[test]
fn t3_lowercase_get_method_is_refused() {
    assert!(parse(&mutant("method = \"GET\"", "method = \"get\"")).is_err());
}

#[test]
fn t4_lua53_language_is_refused() {
    assert!(parse(&mutant("language = \"lua54\"", "language = \"lua53\"")).is_err());
}

#[test]
fn t5_integer_plugin_version_is_refused() {
    assert!(parse(&mutant("plugin_version = \"1.1.0\"", "plugin_version = 2")).is_err());
}

#[test]
fn t6_future_format_version_is_refused_before_anything_else() {
    assert!(matches!(
        parse(&mutant("format_version = 1", "format_version = 3")),
        Err(tidemark_core::plugin::PluginError::UnsupportedFormat { .. })
    ));
}

#[test]
fn t7_tidemark_namespace_is_reserved() {
    assert!(matches!(
        parse(&mutant(
            "id = \"io.github.riccelso.ollama-cloud\"",
            "id = \"tidemark.ollama\""
        )),
        Err(tidemark_core::plugin::PluginError::ReservedId { .. })
    ));
}

#[test]
fn t8_uppercase_id_is_refused() {
    assert!(
        parse(&mutant(
            "id = \"io.github.riccelso.ollama-cloud\"",
            "id = \"IO.GitHub.Riccelso\""
        ))
        .is_err()
    );
}

#[test]
fn t9_dotless_id_is_refused() {
    assert!(
        parse(&mutant(
            "id = \"io.github.riccelso.ollama-cloud\"",
            "id = \"ollamacloud\""
        ))
        .is_err()
    );
}

#[test]
fn t10_non_printable_prefix_is_refused() {
    assert!(
        parse(&mutant(
            "api_key_prefix = \"Bearer \"",
            "api_key_prefix = \"Bearer\\u0007\""
        ))
        .is_err()
    );
}

#[test]
fn t11_source_without_parse_function_is_refused() {
    assert!(
        parse(&mutant(
            "function parse(response, context)",
            "function run(r, c)"
        ))
        .is_err()
    );
}

#[test]
fn t12_scripted_mark_refuses_the_whole_file() {
    // Replace the shipped mark's root with one carrying a <script>: the whole
    // file must be refused at the SVG stage, not merely stripped of it.
    let replaced = FILE.replacen(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 64 64\">",
        "<svg xmlns=\"http://www.w3.org/2000/svg\"><script>x()</script>",
        1,
    );
    assert!(matches!(
        parse(&replaced),
        Err(tidemark_core::plugin::PluginError::Svg { .. })
    ));
}

#[test]
fn t13_an_endpoint_field_is_ignored_not_honoured() {
    // Security property, executable: a stranger's file may not decide where
    // the key goes. There is no endpoint field in the format, and an unknown
    // key is inert — the definition parses and carries no host anywhere.
    let hostile = format!("{FILE}\n[endpoint]\nurl = \"https://evil.example/steal\"\n");
    let definition =
        parse(&hostile).expect("unknown fields are inert; the daemon owns the endpoint");
    let serialized = format!(
        "{}{}{}",
        definition.id, definition.api_key_header, definition.lua_source
    );
    assert!(
        !serialized.contains("evil.example"),
        "the definition carries no host"
    );
}

#[test]
fn t14_header_name_with_space_is_refused() {
    assert!(
        parse(&mutant(
            "api_key_header = \"Authorization\"",
            "api_key_header = \"Authorization \""
        ))
        .is_err()
    );
}

#[test]
fn t15_oversized_provider_name_is_refused() {
    let long = "A".repeat(129);
    assert!(matches!(
        parse(&mutant(
            "name = \"Ollama Cloud\"",
            &format!("name = \"{long}\"")
        )),
        Err(tidemark_core::plugin::PluginError::TooLarge { .. })
    ));
}

#[test]
fn t16_the_shipped_file_itself_parses_clean() {
    let definition = parse(FILE).expect("the shipped file is a valid definition");
    assert_eq!(definition.id, "io.github.riccelso.ollama-cloud");
    assert!(
        definition.icon_svg.is_some(),
        "the mark survived sanitization"
    );
}
