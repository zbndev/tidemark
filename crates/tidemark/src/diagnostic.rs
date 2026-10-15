//! JSON diagnostics: preserve literals, add indentation, and use the application's palette.

use slint::{Color, StyledText};

use crate::markdown;

pub fn message(status: &tidemark_types::ProviderStatus, now: tidemark_types::Timestamp) -> String {
    let mut message = status
        .message
        .as_deref()
        .filter(|message| !message.trim().is_empty())
        .unwrap_or(&status.state)
        .to_owned();
    if let Some(next) = status.next_poll_at {
        let seconds = next.saturating_sub(now.as_unix());
        if seconds > 0 {
            message.push_str(&format!(
                "\nNext automatic check in {}.",
                tidemark_types::duration(seconds)
            ));
        }
    }
    message
}

pub fn highlight(
    body: &str,
    key: Color,
    string: Color,
    number: Color,
    literal: Color,
    punctuation: Color,
) -> StyledText {
    markup(body, key, string, number, literal, punctuation)
        .and_then(|markup| StyledText::from_markdown(&markup).ok())
        .unwrap_or_else(|| StyledText::from_plain_text(body))
}

fn markup(
    body: &str,
    key: Color,
    string: Color,
    number: Color,
    literal: Color,
    punctuation: Color,
) -> Option<String> {
    let tokens = tokens(body)?;
    let mut out = String::new();
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        let previous = index.checked_sub(1).map(|i| tokens[i]);
        let next = tokens.get(index + 1).copied();
        if matches!(*token, "}" | "]") {
            depth -= 1;
            if !matches!(previous, Some("{" | "[")) {
                newline(&mut out, depth);
            }
        }
        let color = if token.starts_with('"') {
            if next == Some(":") { key } else { string }
        } else if matches!(*token, "true" | "false" | "null") {
            literal
        } else if token.starts_with(|c: char| c == '-' || c.is_ascii_digit()) {
            number
        } else {
            punctuation
        };
        out.push_str(&format!(
            "<font color='#{:02x}{:02x}{:02x}{:02x}'>",
            color.red(),
            color.green(),
            color.blue(),
            color.alpha()
        ));
        // A code span prevents response text from becoming markup, including HTML and links.
        markdown::code_span(token, &mut out);
        out.push_str("</font>");
        match *token {
            "{" | "[" => {
                depth += 1;
                if !matches!(next, Some("}" | "]")) {
                    newline(&mut out, depth);
                }
            }
            "," => newline(&mut out, depth),
            ":" => out.push('\u{a0}'),
            _ => {}
        }
    }
    Some(out)
}

fn newline(out: &mut String, depth: usize) {
    out.push_str("\\\n");
    out.extend(std::iter::repeat_n('\u{a0}', depth * 2));
}

/// Validate with serde, then retain the original tokens: reserializing a Value would
/// reorder keys, collapse duplicate keys, and change some numeric spellings.
fn tokens(body: &str) -> Option<Vec<&str>> {
    serde_json::from_str::<serde_json::Value>(body).ok()?;
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        let start = at;
        match bytes[at] {
            b'"' => {
                at += 1;
                while at < bytes.len() {
                    match bytes[at] {
                        b'\\' => at += 2,
                        b'"' => {
                            at += 1;
                            break;
                        }
                        _ => at += 1,
                    }
                }
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => at += 1,
            _ => {
                while at < bytes.len()
                    && !bytes[at].is_ascii_whitespace()
                    && !matches!(bytes[at], b'{' | b'}' | b'[' | b']' | b':' | b',')
                {
                    at += 1;
                }
            }
        }
        out.push(&body[start..at]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlighted_json_preserves_tokens_and_escapes_response_markup() {
        let key = Color::from_rgb_u8(0x99, 0xc1, 0xf1);
        let string = Color::from_rgb_u8(0x78, 0xe9, 0xab);
        let number = Color::from_rgb_u8(0xf8, 0xe4, 0x5c);
        let punctuation = Color::from_argb_u8(217, 255, 255, 255);
        let body = r#"{"key":[{},[],"<font color='red'>`text`</font>",1e+03,true,null]}"#;
        let text = markup(body, key, string, number, key, punctuation).unwrap();
        assert!(text.contains("#99c1f1ff"));
        assert!(text.contains("#78e9abff"));
        assert!(text.contains("#f8e45cff"));
        assert!(text.contains("#ffffffd9"));
        let codes: String = pulldown_cmark::Parser::new(&text)
            .filter_map(|event| match event {
                pulldown_cmark::Event::Code(token) => Some(token.into_string()),
                _ => None,
            })
            .collect();
        assert_eq!(codes, tokens(body).unwrap().concat());
        assert!(StyledText::from_markdown(&text).is_ok());
    }

    #[test]
    fn the_retry_time_comes_from_the_daemon_and_unknown_states_keep_their_message() {
        use tidemark_types::{AccountId, ProviderId, ProviderStatus, Timestamp};
        let mut status = ProviderStatus::pending(&ProviderId::new("claude"), &AccountId::default());
        status.state = "future-state".into();
        let now = Timestamp::from_unix(1_785_700_000).unwrap();
        assert_eq!(message(&status, now), "future-state");
        status.message = Some("provider refused the check".into());
        status.next_poll_at = Some(now.as_unix() + 120);
        assert!(message(&status, now).contains("Next automatic check in 2 min."));
        status.next_poll_at = None;
        assert_eq!(message(&status, now), "provider refused the check");
    }

    #[test]
    fn literals_and_duplicate_keys_survive_formatting() {
        let body = r#"{"z":1e+03,"z":-0,"a":"Привет \\\" <font> `code`","b":[true,null,{},[]]}"#;
        let parsed = tokens(body).expect("valid JSON");
        assert_eq!(parsed.iter().filter(|token| **token == "\"z\"").count(), 2);
        assert!(parsed.contains(&"1e+03"));
        assert!(parsed.contains(&"-0"));
        let compact: String = parsed.concat();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&compact).unwrap(),
            serde_json::from_str::<serde_json::Value>(body).unwrap()
        );
        let blue = Color::from_rgb_u8(0x99, 0xc1, 0xf1);
        assert_ne!(
            highlight(body, blue, blue, blue, blue, blue),
            StyledText::from_plain_text(body)
        );
    }

    #[test]
    fn non_json_and_malformed_json_are_verbatim_text() {
        let color = Color::from_rgb_u8(0xff, 0xff, 0xff);
        for body in [
            "",
            "Too many requests",
            "<html>Forbidden</html>",
            "{ broken JSON",
            "{\"cut\":",
        ] {
            assert_eq!(
                highlight(body, color, color, color, color, color),
                StyledText::from_plain_text(body)
            );
        }
    }
}
