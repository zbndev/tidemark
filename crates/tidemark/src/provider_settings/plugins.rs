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

use adw::prelude::*;
use gtk::gio;
use tidemark_types::{PluginInfo, account_id_suggestion};

use super::{name_suggests_usable, reason};
use crate::bus::DaemonProxy;
use crate::mark;

/// The label/value rows the import dialog shows, in the order they are read.
///
/// The header and the prefix are the point of the whole dialog: together they say exactly
/// where this stranger's file will put the key that is about to be handed to it.
pub(super) fn preview(info: &PluginInfo) -> Vec<(String, String)> {
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

/// What to say before a plain-http endpoint is accepted, or nothing when there is nothing
/// to warn about.
pub(super) fn endpoint_warning(url: &str) -> Option<String> {
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
pub(super) fn valid_endpoint(url: &str) -> bool {
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

/// What the account form was filled in with, in the order the daemon is told it.
#[derive(Debug)]
pub(super) struct AccountForm {
    /// The account id, already suggested from the typed name. `None` when the caller
    /// already knows which account this is for.
    pub(super) slug: Option<String>,
    pub(super) endpoint: String,
    pub(super) allow_insecure_http: bool,
    pub(super) key: String,
}

/// Asks for everything one plugin account needs, in one form.
///
/// Together rather than in three dialogs because they are one decision: the endpoint says
/// where the key goes, and agreeing to the second without seeing the first is the mistake
/// this whole flow exists to prevent. Returns `None` when the dialog was dismissed.
///
/// A second account of an already-configured plugin inherits its sibling's endpoint, so
/// the URL is asked for only when there is nothing to inherit — the first account, or an
/// older daemon that does not publish endpoints.
pub(super) async fn account_dialog(
    parent: &impl IsA<gtk::Widget>,
    info: &PluginInfo,
    heading: &str,
    ask_for_name: bool,
    ask_for_endpoint: bool,
) -> Option<AccountForm> {
    let dialog = adw::AlertDialog::builder().heading(heading).build();
    dialog.add_responses(&[("cancel", "Cancel"), ("accept", "Add")]);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();

    let name = gtk::Entry::builder()
        .placeholder_text("Account name")
        .activates_default(true)
        .build();
    let name_preview = gtk::Label::builder()
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();
    if ask_for_name {
        content.append(&name);
        content.append(&name_preview);
    }

    let endpoint = gtk::Entry::builder()
        .placeholder_text("https://example.com/v1/usage")
        .activates_default(true)
        .build();
    if ask_for_endpoint {
        content.append(&label("Metrics URL"));
        content.append(&endpoint);
    }

    // The one sentence that says where the key is about to be sent, in the shape the
    // request will actually take. It is the whole reason the endpoint and the key are
    // asked for on the same screen.
    content.append(&caption(&format!(
        "{} request, key sent in {}{}",
        info.method,
        info.api_key_header,
        if info.api_key_prefix.is_empty() {
            String::new()
        } else {
            format!(" after {:?}", info.api_key_prefix)
        }
    )));

    let warning = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .visible(false)
        .css_classes(["caption", "warning"])
        .build();
    let insecure = gtk::CheckButton::builder()
        .label("Send the key over plain http")
        .visible(false)
        .build();
    content.append(&warning);
    content.append(&insecure);

    let key = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .activates_default(true)
        .build();
    content.append(&label("API key"));
    content.append(&key);
    dialog.set_extra_child(Some(&content));

    let refresh = {
        let dialog = dialog.clone();
        let name = name.clone();
        let name_preview = name_preview.clone();
        let endpoint = endpoint.clone();
        let warning = warning.clone();
        let insecure = insecure.clone();
        let key = key.clone();
        move || {
            let typed = endpoint.text().to_string();
            let named = if ask_for_name {
                let text = name.text().to_string();
                name_preview.set_text(&format!("Account id: {}", account_id_suggestion(&text)));
                name_suggests_usable(&text, None)
            } else {
                true
            };
            match endpoint_warning(&typed) {
                Some(sentence) => {
                    warning.set_text(&sentence);
                    warning.set_visible(true);
                    insecure.set_visible(true);
                }
                None => {
                    warning.set_visible(false);
                    insecure.set_visible(false);
                    // Cleared rather than left set: an acknowledgement given for a plain
                    // http URL must not survive the URL being changed to https and back.
                    insecure.set_active(false);
                }
            }
            let acknowledged = endpoint_warning(&typed).is_none() || insecure.is_active();
            // An inherited endpoint was already validated by the sibling that carries it;
            // there is nothing typed here to re-validate.
            let endpoint_ok = !ask_for_endpoint || valid_endpoint(&typed);
            dialog.set_response_enabled(
                "accept",
                named && endpoint_ok && acknowledged && !key.text().trim().is_empty(),
            );
        }
    };
    name.connect_changed({
        let refresh = refresh.clone();
        move |_| refresh()
    });
    endpoint.connect_changed({
        let refresh = refresh.clone();
        move |_| refresh()
    });
    key.connect_changed({
        let refresh = refresh.clone();
        move |_| refresh()
    });
    insecure.connect_toggled({
        let refresh = refresh.clone();
        move |_| refresh()
    });
    refresh();

    (dialog.choose_future(Some(parent)).await == "accept").then(|| AccountForm {
        slug: ask_for_name.then(|| account_id_suggestion(&name.text())),
        endpoint: endpoint.text().trim().to_owned(),
        allow_insecure_http: insecure.is_active(),
        key: key.text().trim().to_owned(),
    })
}

/// Shows what a file declares and offers to install it, or reports why it was refused.
///
/// Two steps on purpose: inspecting writes nothing, so a file somebody sent can be read
/// here and then closed. Returns the installed definition's metadata, or `None` when
/// nothing was installed.
pub(super) async fn import_dialog(
    parent: &impl IsA<gtk::Widget>,
    proxy: &DaemonProxy<'static>,
    bytes: Vec<u8>,
) -> Option<PluginInfo> {
    let info = match proxy.inspect_plugin(bytes.clone()).await {
        Ok(info) => info,
        Err(error) => {
            let refusal = adw::AlertDialog::builder()
                .heading("This file was refused")
                .body(reason(&error))
                .build();
            refusal.add_response("close", "Close");
            refusal.set_close_response("close");
            refusal.choose_future(Some(parent)).await;
            return None;
        }
    };

    let dialog = adw::AlertDialog::builder()
        .heading(format!("Install {}?", info.name))
        .body(
            "This provider is not part of Tidemark. It runs a parser the file's author wrote, and it will be sent the API key you give it.",
        )
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("install", "Install")]);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    if info.has_mark {
        let image = mark::image();
        if let Some(svg) = info.mark_svg.as_deref() {
            mark::set_preview(&image, svg);
        } else {
            // Compatibility with an older daemon: installed plugins only named their
            // materialized mark, which still works when the icon is already on disk.
            mark::set(&image, &info.id);
        }
        image.set_halign(gtk::Align::Center);
        content.append(&image);
    }
    let rows = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(4)
        .build();
    for (index, (title, value)) in preview(&info).into_iter().enumerate() {
        let row = i32::try_from(index).unwrap_or(i32::MAX);
        rows.attach(&caption(&title), 0, row, 1, 1);
        let value = gtk::Label::builder()
            .label(&value)
            .xalign(0.0)
            .selectable(true)
            .wrap(true)
            .build();
        rows.attach(&value, 1, row, 1, 1);
    }
    content.append(&rows);
    dialog.set_extra_child(Some(&content));

    if dialog.choose_future(Some(parent)).await != "install" {
        return None;
    }
    match proxy.install_plugin(bytes).await {
        Ok(installed) => Some(installed),
        Err(error) => {
            let refusal = adw::AlertDialog::builder()
                .heading("This file was refused")
                .body(reason(&error))
                .build();
            refusal.add_response("close", "Close");
            refusal.set_close_response("close");
            refusal.choose_future(Some(parent)).await;
            None
        }
    }
}

/// The file chooser, filtered to the one extension a definition has.
pub(super) fn file_dialog() -> gtk::FileDialog {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Tidemark provider"));
    filter.add_pattern("*.tidemark-provider");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    gtk::FileDialog::builder()
        .title("Import a provider")
        .filters(&filters)
        .default_filter(&filter)
        .modal(true)
        .build()
}

fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["heading"])
        .build()
}

fn caption(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["caption", "dim-label"])
        .build()
}

#[cfg(test)]
mod tests {
    use super::{endpoint_warning, preview, valid_endpoint};
    use tidemark_types::PluginInfo;

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
        assert_eq!(
            prefix.1, "none",
            "a missing prefix is a fact about the request, not a blank"
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
