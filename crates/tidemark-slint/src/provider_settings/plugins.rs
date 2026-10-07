//! Importing a user-installed provider, and pointing one of its accounts at a URL.
//!
//! A plugin is somebody else's file, and the two questions that matter before it is
//! trusted are *where does my key go* and *where is it sent*. Both are answered on screen
//! before anything is installed and before any key is stored, which is what the preview
//! rows and the endpoint form are for.
//!
//! Nothing here decides whether a definition is acceptable. The daemon parses the file,
//! compiles its Lua and refuses the endpoints it refuses; these functions only shape what
//! is shown and keep the confirm button off an input that has no chance. A second opinion
//! living in the client is a second opinion that goes stale.

use std::path::PathBuf;

use tidemark_types::PluginInfo;

/// The label/value rows the import dialog shows, in the order they are read.
///
/// The header and the prefix are the point of the whole dialog: together they say exactly
/// where this stranger's file will put the key that is about to be handed to it.
pub fn preview(info: &PluginInfo) -> Vec<(String, String)> {
    vec![
        ("Provider id".to_owned(), info.id.clone()),
        ("Name".to_owned(), info.name.clone()),
        ("Plugin version".to_owned(), info.plugin_version.clone()),
        ("Request".to_owned(), info.method.clone()),
        ("Key header".to_owned(), info.api_key_header.clone()),
        ("Key prefix".to_owned(), prefix(&info.api_key_prefix)),
    ]
}

/// An empty prefix shown as the absence it is.
///
/// Left blank the row reads as a value that failed to load; "none" is a fact about the
/// request — the key is sent with nothing in front of it.
fn prefix(prefix: &str) -> String {
    if prefix.is_empty() {
        "none".to_owned()
    } else {
        prefix.to_owned()
    }
}

/// The one sentence that says where the key is about to be sent, in the shape the request
/// will actually take. It is the whole reason the endpoint and the key are asked for on
/// the same screen.
pub fn request_line(info: &PluginInfo) -> String {
    format!(
        "{} request, key sent in {}{}",
        info.method,
        info.api_key_header,
        if info.api_key_prefix.is_empty() {
            String::new()
        } else {
            format!(" after {:?}", info.api_key_prefix)
        }
    )
}

/// What to say before a plain-http endpoint is accepted, or nothing when there is nothing
/// to warn about.
pub fn endpoint_warning(url: &str) -> Option<String> {
    url.trim().starts_with("http://").then(|| {
        "This endpoint is plain http. The API key will cross the network in clear, where \
         anything between this machine and the endpoint can read it."
            .to_owned()
    })
}

/// Whether an endpoint is worth offering to send at all.
///
/// Deliberately the daemon's *refusals* and not its acceptances: it parses the URL, and it
/// is the one that answers. This only keeps the confirm button off an input that has no
/// chance — a bare host, a path with no origin, a scheme that is not http, credentials in
/// the URL, a fragment — so that the obvious mistakes are caught while typing rather than
/// as an error toast after a round trip.
pub fn valid_endpoint(url: &str) -> bool {
    let url = url.trim();
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    if url.contains('#') {
        return false;
    }
    let authority = rest
        .split(['/', '?'])
        .next()
        .expect("splitting a string yields at least one piece");
    !authority.is_empty() && !authority.contains('@')
}

/// The system's file chooser, filtered to the one extension a definition has: the XDG
/// portal on Linux, the common dialog on Windows. `None` when it was dismissed.
pub async fn choose_file() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Import a provider")
        .add_filter("Tidemark provider", &["tidemark-provider"])
        .pick_file()
        .await
        .map(|file| file.path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(header: &str, prefix: &str, method: &str) -> PluginInfo {
        PluginInfo {
            id: "com.acme.quota".into(),
            name: "Acme AI".into(),
            plugin_version: "1.0.0".into(),
            method: method.into(),
            api_key_header: header.into(),
            api_key_prefix: prefix.into(),
            has_mark: true,
            mark_svg: None,
        }
    }

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
        let prefix = rows
            .iter()
            .find(|(label, _)| label == "Key prefix")
            .expect("shown");
        assert_eq!(prefix.1, "none");
    }

    #[test]
    fn the_request_line_names_the_header_and_any_prefix() {
        assert_eq!(
            request_line(&info("Authorization", "Bearer ", "POST")),
            "POST request, key sent in Authorization after \"Bearer \""
        );
        assert_eq!(
            request_line(&info("X-Acme-Key", "", "GET")),
            "GET request, key sent in X-Acme-Key"
        );
    }

    #[test]
    fn a_plain_http_endpoint_warns_about_the_key_before_it_is_accepted() {
        let warning = endpoint_warning("http://metrics.corp.test/u").expect("warns");
        assert!(warning.contains("key"), "{warning}");
        assert_eq!(endpoint_warning("https://metrics.corp.test/u"), None);
    }

    #[test]
    fn the_confirm_button_is_only_enabled_for_an_endpoint_the_daemon_would_accept() {
        assert!(valid_endpoint(
            "https://metrics.corp.test/v1/usage?window=month"
        ));
        assert!(valid_endpoint("http://metrics.corp.test/u"));
        for bad in [
            "",
            "  ",
            "metrics.corp.test/u",
            "/v1/usage",
            "ftp://a.test/u",
            "https://user:pw@a.test/u",
            "https://a.test/u#frag",
        ] {
            assert!(
                !valid_endpoint(bad),
                "{bad} would be refused, so do not offer to send it"
            );
        }
    }
}
