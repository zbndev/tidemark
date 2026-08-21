//! The credentials dialog: one group per account, and the two ways in.
//!
//! Everything here is drawn from what the daemon published. The dialog knows that some
//! accounts take a key and some sign in through a browser, and it knows how to draw a
//! choice between named alternatives — it does not know what Z.ai is, what a region means,
//! or which providers exist. Adding a provider adds a group to this dialog by registering
//! it in the daemon, and changes nothing in this file.
//!
//! # What is never shown
//!
//! A stored key. The field starts empty and stays empty; the row says whether something is
//! stored, and the way to replace it is to type a new one. There is nothing to gain from
//! putting a secret back on screen — the daemon would have to hand it out over the bus to
//! do it — and the only question a user has here is "does Tidemark have one".
//!
//! # Why the browser is opened from this process
//!
//! The daemon builds the URL and receives the redirect; the interface opens the URL. A
//! background service has no reliable way to reach a desktop it may have been started
//! before, and `GtkUriLauncher` goes through the portal on the session that is actually
//! there. A browser that does not open leaves the user with a Copy link button rather than
//! a spinner and no explanation — the address itself is never rendered, both because an
//! authorize URL is three lines of query string and because `AdwActionRow` parses its
//! subtitle as Pango markup, which an unescaped `&` in a query string is not.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tidemark_types::{CredentialKind, ProviderStatus, Remedy, provider_label};

use crate::bus::DaemonProxy;
use crate::format;

/// The dialog, and the rows it has to keep up to date.
#[derive(Debug)]
pub struct Credentials {
    dialog: adw::PreferencesDialog,
    proxy: DaemonProxy<'static>,
    accounts: RefCell<Vec<AccountRows>>,
}

/// The widgets of one account that depend on the published status.
#[derive(Debug)]
struct AccountRows {
    provider: String,
    account: String,
    group: adw::PreferencesGroup,
    /// The row that says what the credential currently is, for a signing-in account.
    sign_in: Option<SignInRow>,
    /// The row that says whether a key is stored.
    key: Option<adw::PasswordEntryRow>,
    /// The row that removes a stored key, shown only while there is one to remove.
    remove: Option<adw::ActionRow>,
    /// Whether a login is currently waiting on this account's browser.
    waiting: std::cell::Cell<bool>,
}

/// The one row of an OAuth account, and the button that changes.
#[derive(Debug)]
struct SignInRow {
    row: adw::ActionRow,
    button: gtk::Button,
    stack: gtk::Stack,
    /// The address of the login currently waiting, for the Copy link button.
    url: Rc<RefCell<String>>,
}

impl Credentials {
    /// Builds the dialog over what the daemon currently says, and presents it.
    pub fn present(
        parent: &impl IsA<gtk::Widget>,
        proxy: DaemonProxy<'static>,
        statuses: &[ProviderStatus],
    ) -> Rc<Self> {
        let page = adw::PreferencesPage::new();
        let dialog = adw::PreferencesDialog::builder()
            .title("Providers")
            .content_width(560)
            .content_height(680)
            .build();
        dialog.add(&page);

        let credentials = Rc::new(Self {
            dialog: dialog.clone(),
            proxy,
            accounts: RefCell::new(Vec::new()),
        });

        for status in statuses {
            let rows = credentials.build_group(status);
            page.add(&rows.group);
            credentials.accounts.borrow_mut().push(rows);
        }
        credentials.apply(statuses);

        dialog.present(Some(parent));
        credentials
    }

    /// Brings the parts that depend on the daemon up to date, leaving anything the user is
    /// in the middle of typing alone.
    pub fn apply(&self, statuses: &[ProviderStatus]) {
        for rows in self.accounts.borrow().iter() {
            let Some(status) = statuses
                .iter()
                .find(|s| s.provider == rows.provider && s.account == rows.account)
            else {
                continue;
            };
            rows.group.set_description(Some(&describe(status)));
            let stored_key = status.has_credential == Some(true);
            if let Some(key) = &rows.key {
                key.set_title(if stored_key {
                    "Replace the stored key"
                } else {
                    "API key"
                });
            }
            if let Some(remove) = &rows.remove {
                // Nothing stored, nothing to remove. A permanently visible destructive
                // button over an empty account is an invitation to wonder what it deletes.
                remove.set_visible(stored_key);
            }
            if let Some(sign_in) = &rows.sign_in
                && !rows.waiting.get()
            {
                let stored = status.has_credential == Some(true);
                sign_in.row.set_subtitle(if stored {
                    "Signed in to Tidemark."
                } else {
                    "Using the CLI's own login."
                });
                sign_in
                    .button
                    .set_label(if stored { "Sign out" } else { "Sign in…" });
                sign_in.button.set_sensitive(true);
                sign_in.stack.set_visible_child_name(BUTTON);
            }
        }
    }

    /// Whether this dialog is still on screen, so a stale one is dropped rather than fed.
    pub fn is_open(&self) -> bool {
        self.dialog.is_visible()
    }

    fn build_group(self: &Rc<Self>, status: &ProviderStatus) -> AccountRows {
        let group = adw::PreferencesGroup::builder()
            .title(provider_label(&status.provider))
            .build();

        let mut rows = AccountRows {
            provider: status.provider.clone(),
            account: status.account.clone(),
            group: group.clone(),
            sign_in: None,
            key: None,
            remove: None,
            waiting: std::cell::Cell::new(false),
        };

        match status.credential_kind() {
            Some(CredentialKind::Key) => {
                let (entry, remove) = self.build_key_rows(status, &group);
                rows.key = Some(entry);
                rows.remove = Some(remove);
            }
            Some(CredentialKind::OAuth) => {
                rows.sign_in = Some(self.build_sign_in_row(status, &group));
            }
            // Nothing to enter and nothing to remove. The hint in the group's description
            // is the whole of what this account has to say, so the group carries no rows
            // rather than a row that does nothing.
            Some(CredentialKind::External) | None => {}
        }

        for option in &status.options {
            let row = self.build_option_row(status, option);
            group.add(&row);
            if let Some(description) = &option.description {
                // Under the row rather than inside it. A `AdwComboRow` shows its current
                // value at the end of the row, and a sentence in the subtitle squeezes that
                // value down to an ellipsis — which hides the one thing the control exists
                // to report.
                group.add(&caption(description));
            }
        }
        rows
    }

    fn build_key_rows(
        self: &Rc<Self>,
        status: &ProviderStatus,
        group: &adw::PreferencesGroup,
    ) -> (adw::PasswordEntryRow, adw::ActionRow) {
        let entry = adw::PasswordEntryRow::builder().title("API key").build();
        let save = gtk::Button::builder()
            .label("Save")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .css_classes(["suggested-action"])
            .build();
        entry.add_suffix(&save);

        // Nothing to save until something is typed, so the button says so rather than
        // storing a blank the daemon would only refuse.
        entry.connect_changed({
            let save = save.clone();
            move |entry| save.set_sensitive(!entry.text().trim().is_empty())
        });

        let store = {
            let this = Rc::clone(self);
            let entry = entry.clone();
            let provider = status.provider.clone();
            let account = status.account.clone();
            move || {
                let key = entry.text().trim().to_owned();
                if key.is_empty() {
                    return;
                }
                entry.set_text("");
                let this = Rc::clone(&this);
                let provider = provider.clone();
                let account = account.clone();
                glib::spawn_future_local(async move {
                    match this.proxy.set_key(&provider, &account, &key).await {
                        Ok(()) => this.toast("Key saved. Checking the account…"),
                        Err(error) => this.toast(&reason(&error)),
                    }
                });
            }
        };
        save.connect_clicked({
            let store = store.clone();
            move |_| store()
        });
        entry.connect_entry_activated(move |_| store());

        group.add(&entry);
        let remove = self.build_remove_row(status);
        group.add(&remove);
        (entry, remove)
    }

    fn build_remove_row(self: &Rc<Self>, status: &ProviderStatus) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title("Stored key")
            .subtitle("Removing it leaves the account with no credential.")
            .build();
        let remove = gtk::Button::builder()
            .label("Remove")
            .valign(gtk::Align::Center)
            .css_classes(["destructive-action"])
            .build();
        remove.connect_clicked({
            let this = Rc::clone(self);
            let provider = status.provider.clone();
            let account = status.account.clone();
            move |_| {
                let this = Rc::clone(&this);
                let provider = provider.clone();
                let account = account.clone();
                glib::spawn_future_local(async move {
                    match this.proxy.sign_out(&provider, &account).await {
                        Ok(()) => this.toast("Key removed."),
                        Err(error) => this.toast(&reason(&error)),
                    }
                });
            }
        });
        row.add_suffix(&remove);
        row
    }

    fn build_sign_in_row(
        self: &Rc<Self>,
        status: &ProviderStatus,
        group: &adw::PreferencesGroup,
    ) -> SignInRow {
        let row = adw::ActionRow::builder()
            .title("Tidemark's own account")
            .build();
        let button = gtk::Button::builder()
            .label("Sign in…")
            .valign(gtk::Align::Center)
            .build();
        let cancel = gtk::Button::builder()
            .label("Cancel")
            .valign(gtk::Align::Center)
            .build();
        let copy = gtk::Button::builder()
            .label("Copy link")
            .valign(gtk::Align::Center)
            .build();
        let spinner = adw::Spinner::builder()
            .width_request(16)
            .height_request(16)
            .build();

        let waiting = gtk::Box::builder().spacing(8).build();
        waiting.append(&spinner);
        waiting.append(&copy);
        waiting.append(&cancel);

        let url: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        copy.connect_clicked({
            let this = Rc::clone(self);
            let url = Rc::clone(&url);
            move |button| {
                let address = url.borrow().clone();
                if address.is_empty() {
                    return;
                }
                button.clipboard().set_text(&address);
                this.toast("Login address copied. Open it in a browser.");
            }
        });

        // A stack rather than two rows: the button and the spinner occupy the same place,
        // so the row does not change height when a login starts.
        let stack = gtk::Stack::builder()
            .valign(gtk::Align::Center)
            .hhomogeneous(false)
            .build();
        stack.add_named(&button, Some(BUTTON));
        stack.add_named(&waiting, Some(WAITING));
        row.add_suffix(&stack);
        group.add(&row);

        let provider = status.provider.clone();
        let account = status.account.clone();
        button.connect_clicked({
            let this = Rc::clone(self);
            let provider = provider.clone();
            let account = account.clone();
            move |button| {
                let signed_in = button.label().is_some_and(|label| label == "Sign out");
                let this = Rc::clone(&this);
                let provider = provider.clone();
                let account = account.clone();
                if signed_in {
                    glib::spawn_future_local(async move {
                        match this.proxy.sign_out(&provider, &account).await {
                            Ok(()) => this.toast("Signed out of Tidemark's account."),
                            Err(error) => this.toast(&reason(&error)),
                        }
                    });
                } else {
                    glib::spawn_future_local(async move { this.sign_in(provider, account).await });
                }
            }
        });
        cancel.connect_clicked({
            let this = Rc::clone(self);
            move |_| {
                let this = Rc::clone(&this);
                let provider = provider.clone();
                let account = account.clone();
                glib::spawn_future_local(async move {
                    let _ = this.proxy.cancel_login(&provider, &account).await;
                });
            }
        });

        SignInRow {
            row,
            button,
            stack,
            url,
        }
    }

    fn build_option_row(
        self: &Rc<Self>,
        status: &ProviderStatus,
        option: &tidemark_types::ProviderOption,
    ) -> adw::ComboRow {
        let titles: Vec<&str> = option
            .choices
            .iter()
            .map(|choice| choice.title.as_str())
            .collect();
        let selected = option
            .choices
            .iter()
            .position(|choice| choice.value == option.value)
            .unwrap_or(0) as u32;
        let row = adw::ComboRow::builder()
            .title(&option.title)
            .model(&gtk::StringList::new(&titles))
            .selected(selected)
            // The selected value goes in the subtitle, where it has the width of the row
            // rather than whatever is left at the end of it.
            .use_subtitle(true)
            // Without an expression the row has no way to turn a `GtkStringObject` into
            // the words to put there, and the subtitle comes out empty — which loses the
            // one thing the control is meant to report.
            .expression(gtk::PropertyExpression::new(
                gtk::StringObject::static_type(),
                None::<gtk::Expression>,
                "string",
            ))
            .build();

        let values: Vec<String> = option
            .choices
            .iter()
            .map(|choice| choice.value.clone())
            .collect();
        let current = RefCell::new(option.value.clone());
        row.connect_selected_notify({
            let this = Rc::clone(self);
            let provider = status.provider.clone();
            let account = status.account.clone();
            let name = option.name.clone();
            move |row| {
                let Some(chosen) = values.get(row.selected() as usize).cloned() else {
                    return;
                };
                // `selected` also fires when the row is built and when nothing moved;
                // writing the file on either would rewrite it for no reason.
                if *current.borrow() == chosen {
                    return;
                }
                *current.borrow_mut() = chosen.clone();
                let this = Rc::clone(&this);
                let provider = provider.clone();
                let account = account.clone();
                let name = name.clone();
                glib::spawn_future_local(async move {
                    if let Err(error) = this
                        .proxy
                        .set_option(&provider, &account, &name, &chosen)
                        .await
                    {
                        this.toast(&reason(&error));
                    }
                });
            }
        });
        row
    }

    /// The whole login, from this side: ask for a URL, open it, wait.
    async fn sign_in(self: Rc<Self>, provider: String, account: String) {
        let url = match self.proxy.begin_login(&provider, &account).await {
            Ok(url) => url,
            Err(error) => {
                self.toast(&reason(&error));
                return;
            }
        };
        self.set_waiting(&provider, &account, Some(&url));

        // Opened after the daemon is already listening, never before: a browser that came
        // back first would find nothing on the port.
        let launcher = gtk::UriLauncher::new(&url);
        if let Err(error) = launcher
            .launch_future(self.dialog.root().and_downcast_ref::<gtk::Window>())
            .await
        {
            tracing::warn!(%error, "could not open the browser");
            self.toast("Could not open a browser. Use Copy link and open it yourself.");
        }

        let outcome = self.proxy.await_login(&provider, &account).await;
        self.set_waiting(&provider, &account, None);
        match outcome {
            Ok(()) => self.toast("Signed in. Checking the account…"),
            Err(error) => self.toast(&reason(&error)),
        }
    }

    /// Puts one account's sign-in row into, or out of, the waiting state.
    fn set_waiting(&self, provider: &str, account: &str, url: Option<&str>) {
        let accounts = self.accounts.borrow();
        let Some(rows) = accounts
            .iter()
            .find(|rows| rows.provider == provider && rows.account == account)
        else {
            return;
        };
        rows.waiting.set(url.is_some());
        let Some(sign_in) = &rows.sign_in else {
            return;
        };
        match url {
            Some(url) => {
                *sign_in.url.borrow_mut() = url.to_owned();
                sign_in.row.set_subtitle("Waiting for your browser…");
                sign_in.stack.set_visible_child_name(WAITING);
            }
            None => {
                sign_in.url.borrow_mut().clear();
                sign_in.stack.set_visible_child_name(BUTTON);
            }
        }
    }

    fn toast(&self, message: &str) {
        self.dialog.add_toast(adw::Toast::new(message));
    }
}

/// A sentence under a row, in the quiet style libadwaita uses for one.
fn caption(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .wrap(true)
        .margin_top(6)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["caption", "dim-label"])
        .build()
}

/// Names of the two things that can be at the end of a sign-in row.
const BUTTON: &str = "button";
const WAITING: &str = "waiting";

/// The sentence under an account's name: what is wrong, if anything, and then where the
/// credential comes from.
///
/// The state comes first when there is one, because a user opening this dialog opened it
/// for a reason, and the reason is on the card they were looking at.
fn describe(status: &ProviderStatus) -> String {
    let hint = status.credential_hint.clone().unwrap_or_default();
    match format::chip(status) {
        Some(chip)
            if status
                .state()
                .is_none_or(|state| state.remedy() != Remedy::Nothing) =>
        {
            let detail = status.message.clone().unwrap_or(chip.text);
            if hint.is_empty() {
                detail
            } else {
                format!("{detail} — {hint}")
            }
        }
        _ => hint,
    }
}

/// A D-Bus error as one sentence for a toast.
///
/// The daemon's own message is what is shown: it is the process that knows whether the
/// keyring was locked or the provider said no, and any rewording here would lose that.
fn reason(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(_, Some(detail), _) => detail.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, ProviderState};

    fn status(kind: CredentialKind) -> ProviderStatus {
        let mut status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        status.credential = Some(kind.as_wire().to_owned());
        status.credential_hint = Some("Z.ai dashboard → API keys.".into());
        status
    }

    #[test]
    fn a_healthy_account_is_described_by_where_its_credential_comes_from() {
        let mut healthy = status(CredentialKind::Key);
        healthy.set_state(ProviderState::Ok, None);
        assert_eq!(describe(&healthy), "Z.ai dashboard → API keys.");
    }

    #[test]
    fn an_account_the_user_must_fix_leads_with_what_is_wrong() {
        let mut rejected = status(CredentialKind::Key);
        rejected.set_state(
            ProviderState::CredentialRejected,
            Some("the credential was rejected (HTTP 401)".into()),
        );
        assert_eq!(
            describe(&rejected),
            "the credential was rejected (HTTP 401) — Z.ai dashboard → API keys."
        );
    }

    #[test]
    fn an_account_that_is_merely_waiting_does_not_shout_about_it() {
        // `pending` is the state every account has at startup, and the dialog opening a
        // second after the daemon did must not read as five problems.
        let waiting = status(CredentialKind::Key);
        assert_eq!(describe(&waiting), "Z.ai dashboard → API keys.");
    }

    #[test]
    fn a_state_this_build_does_not_know_is_still_reported() {
        let mut unknown = status(CredentialKind::OAuth);
        unknown.state = "quota-frozen".into();
        assert!(
            describe(&unknown).starts_with("quota-frozen"),
            "{}",
            describe(&unknown)
        );
    }
}
