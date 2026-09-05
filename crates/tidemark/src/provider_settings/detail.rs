use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tidemark_types::{
    AuthSelection, CredentialKind, ExternalLogin, ProviderDefinition, ProviderOption,
    ProviderStatus, Remedy,
};

use super::browser_auth::BrowserAuth;
use super::model::AuthSource;
use super::{DEFAULT_ACCOUNT, model, name_dialog, reason};
use crate::bus::DaemonProxy;
use crate::{format, mark};

const BUTTON: &str = "button";
const WAITING: &str = "waiting";

type WaitingCallback = Rc<dyn Fn(String, String, bool) -> bool>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterBeginAction {
    OpenBrowserAndAwait,
    CancelLogin,
}

pub(super) fn after_begin_action(accepted: bool) -> AfterBeginAction {
    if accepted {
        AfterBeginAction::OpenBrowserAndAwait
    } else {
        AfterBeginAction::CancelLogin
    }
}

/// One provider's stable detail page. It is cached by the dialog so navigation never
/// discards text being entered or a browser login in progress.
pub(super) struct ProviderDetail {
    dialog: adw::PreferencesDialog,
    proxy: DaemonProxy<'static>,
    definition: ProviderDefinition,
    status: RefCell<ProviderStatus>,
    page: adw::NavigationPage,
    header: adw::HeaderBar,
    authentication: adw::PreferencesGroup,
    key: RefCell<Option<KeyRows>>,
    sign_in: RefCell<Option<SignInRow>>,
    /// The CLI half, for a provider whose credential can come from another program here.
    local: RefCell<Option<LocalRows>>,
    /// The pill that picks between the two halves. Absent for a provider with one.
    choice: RefCell<Option<SourceChoice>>,
    /// The tabs and source rows of an explicitly chosen local login. Absent when the
    /// daemon published no such selector for this provider.
    browser_auth: RefCell<Option<BrowserAuth>>,
    external: RefCell<Option<adw::ActionRow>>,
    options: RefCell<BTreeMap<String, Rc<OptionRow>>>,
    notifications: adw::PreferencesGroup,
    notify_rows: RefCell<Vec<Rc<NotifyRow>>>,
    waiting: Cell<bool>,
    on_waiting: WaitingCallback,
}

#[derive(Debug)]
struct KeyRows {
    entry: adw::PasswordEntryRow,
    save: gtk::Button,
    remove: adw::ActionRow,
}

#[derive(Debug)]
struct SignInRow {
    row: adw::ActionRow,
    button: gtk::Button,
    stack: gtk::Stack,
    url: Rc<RefCell<String>>,
}

/// The rows of the half that reads a login another program on this machine holds.
///
/// Built once and hidden, like the sign-in row beside it: the two halves are one group,
/// and rebuilding a group under a pointer that is on it loses the click.
#[derive(Debug)]
struct LocalRows {
    /// Where the login is, and whether it is there.
    found: adw::ActionRow,
    /// `Found` or `Not found`, blank while the daemon has not said.
    presence: gtk::Label,
    /// What to run to create one. Absent when there is no single command to name.
    command: Option<adw::ActionRow>,
    /// That Tidemark writes to a file it does not own. Absent when it only reads.
    note: Option<gtk::Label>,
}

impl LocalRows {
    fn set_visible(&self, visible: bool) {
        self.found.set_visible(visible);
        if let Some(command) = &self.command {
            command.set_visible(visible);
        }
        if let Some(note) = &self.note {
            note.set_visible(visible);
        }
    }
}

/// The credential pill, told apart from the daemon's answer about which half is in force.
///
/// The same discipline as [`OptionSelection`], for the same reason: the pill moves the
/// moment it is clicked, the write happens afterwards, and a refused write must put it
/// back without that looking like a second click. What is different here is that the two
/// halves are two screens, so a rollback moves the rows as well as the pill.
#[derive(Debug)]
struct SourceChoice {
    /// The setting a click writes, as the daemon named it.
    option: String,
    group: adw::ToggleGroup,
    authoritative: Cell<AuthSource>,
    suppress: Cell<bool>,
}

impl SourceChoice {
    /// The write this click asks for, or `None` when the pill was moved by us — or moved
    /// to something this build has no screen for, which a daemon newer than it could do.
    fn clicked(&self) -> Option<AuthSource> {
        let chosen = AuthSource::from_value(&self.group.active_name()?)?;
        (!self.suppress.get()).then_some(chosen)
    }

    fn apply_authoritative(&self, source: AuthSource) {
        self.authoritative.set(source);
        self.set_without_signal(source);
    }

    fn rollback(&self) -> AuthSource {
        let authoritative = self.authoritative.get();
        self.set_without_signal(authoritative);
        authoritative
    }

    fn set_without_signal(&self, source: AuthSource) {
        self.suppress.set(true);
        self.group.set_active_name(Some(source.as_value()));
        self.suppress.set(false);
    }
}

#[derive(Debug)]
struct OptionRow {
    row: adw::ComboRow,
    selection: OptionSelection,
}

#[derive(Debug)]
struct OptionSelection {
    values: Vec<String>,
    authoritative: RefCell<String>,
    displayed: Cell<u32>,
    suppress: Cell<bool>,
}

impl OptionSelection {
    fn new(values: Vec<String>, authoritative: String, displayed: u32) -> Self {
        Self {
            values,
            authoritative: RefCell::new(authoritative),
            displayed: Cell::new(displayed),
            suppress: Cell::new(false),
        }
    }

    fn selection_changed(&self, selected: u32) -> Option<String> {
        self.displayed.set(selected);
        let chosen = self.values.get(selected as usize)?;
        (!self.suppress.get()).then(|| chosen.clone())
    }

    fn apply_authoritative(&self, value: &str, select: impl FnOnce(u32)) {
        let Some(index) = self.values.iter().position(|candidate| candidate == value) else {
            return;
        };
        *self.authoritative.borrow_mut() = value.to_owned();
        self.select_without_signal(index as u32, select);
    }

    fn rollback(&self, select: impl FnOnce(u32)) {
        let authoritative = self.authoritative.borrow();
        let Some(index) = self
            .values
            .iter()
            .position(|candidate| candidate == authoritative.as_str())
        else {
            return;
        };
        drop(authoritative);
        self.select_without_signal(index as u32, select);
    }

    fn select_without_signal(&self, selected: u32, select: impl FnOnce(u32)) {
        self.suppress.set(true);
        self.displayed.set(selected);
        select(selected);
        self.suppress.set(false);
    }

    #[cfg(test)]
    fn displayed_value(&self) -> Option<&str> {
        self.values
            .get(self.displayed.get() as usize)
            .map(String::as_str)
    }
}

/// One notification switch, told apart from the daemon's answer about it.
///
/// The same discipline as [`OptionSelection`], for the same reason: the widget moves the
/// moment it is clicked, the write happens afterwards, and a refused write or a status
/// arriving mid-flight must put the switch back without that looking like a second click.
#[derive(Debug)]
struct SwitchState {
    authoritative: Cell<bool>,
    displayed: Cell<bool>,
    suppress: Cell<bool>,
}

impl SwitchState {
    fn new(active: bool) -> Self {
        Self {
            authoritative: Cell::new(active),
            displayed: Cell::new(active),
            suppress: Cell::new(false),
        }
    }

    /// The write this toggle asks for, or `None` when the widget was moved by us.
    fn toggled(&self, active: bool) -> Option<bool> {
        self.displayed.set(active);
        (!self.suppress.get()).then_some(active)
    }

    #[cfg(test)]
    fn displayed(&self) -> bool {
        self.displayed.get()
    }

    fn apply_authoritative(&self, active: bool, set: impl FnOnce(bool)) {
        self.authoritative.set(active);
        self.set_without_signal(active, set);
    }

    fn rollback(&self, set: impl FnOnce(bool)) {
        self.set_without_signal(self.authoritative.get(), set);
    }

    fn set_without_signal(&self, active: bool, set: impl FnOnce(bool)) {
        self.suppress.set(true);
        self.displayed.set(active);
        set(active);
        self.suppress.set(false);
    }
}

/// One row of the notifications group, and the window it switches.
#[derive(Debug)]
struct NotifyRow {
    key: String,
    row: adw::SwitchRow,
    state: SwitchState,
}

impl NotifyRow {
    fn apply_authoritative(&self, active: bool) {
        self.state
            .apply_authoritative(active, |active| self.row.set_active(active));
    }

    fn rollback(&self) {
        self.state.rollback(|active| self.row.set_active(active));
    }
}

impl OptionRow {
    fn apply_authoritative(&self, value: &str) {
        self.selection
            .apply_authoritative(value, |selected| self.row.set_selected(selected));
    }

    fn rollback(&self) {
        self.selection
            .rollback(|selected| self.row.set_selected(selected));
    }
}

impl std::fmt::Debug for ProviderDetail {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderDetail")
            .field("definition", &self.definition)
            .field("status", &self.status)
            .field("page", &self.page)
            .finish_non_exhaustive()
    }
}

impl ProviderDetail {
    pub(super) fn new(
        dialog: &adw::PreferencesDialog,
        proxy: DaemonProxy<'static>,
        definition: ProviderDefinition,
        status: ProviderStatus,
        on_waiting: WaitingCallback,
    ) -> Rc<Self> {
        let image = mark::image_at(72);
        mark::set(&image, &definition.provider);
        let title = adw::WindowTitle::new(&definition.title, "");
        // The account's own name under the provider's, when this page is not the default
        // account's: a page a second account lands on must say which account it is.
        if status.account != DEFAULT_ACCOUNT {
            title.set_subtitle(
                status
                    .account_label
                    .as_deref()
                    .unwrap_or(status.account.as_str()),
            );
        }
        let heading = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .halign(gtk::Align::Center)
            .build();
        heading.append(&image);
        heading.append(&title);

        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&heading));

        let authentication = adw::PreferencesGroup::builder()
            .title("Authentication")
            .build();
        let options = adw::PreferencesGroup::builder().title("Settings").build();
        let notifications = adw::PreferencesGroup::builder()
            .title("Notifications")
            .description("Warn at 70% and 90%, and say when a window resets.")
            .visible(false)
            .build();
        let preferences = adw::PreferencesPage::new();
        preferences.add(&authentication);
        preferences.add(&options);
        preferences.add(&notifications);
        let toolbar = adw::ToolbarView::builder().content(&preferences).build();
        toolbar.add_top_bar(&header);
        let page = adw::NavigationPage::new(&toolbar, &definition.title);

        let detail = Rc::new(Self {
            dialog: dialog.clone(),
            proxy,
            definition,
            status: RefCell::new(status),
            page,
            header,
            authentication,
            key: RefCell::new(None),
            sign_in: RefCell::new(None),
            local: RefCell::new(None),
            choice: RefCell::new(None),
            browser_auth: RefCell::new(None),
            external: RefCell::new(None),
            options: RefCell::new(BTreeMap::new()),
            notifications,
            notify_rows: RefCell::new(Vec::new()),
            waiting: Cell::new(false),
            on_waiting,
        });
        detail.build_authentication();
        detail.build_options(&options);
        detail.wire_rename();
        let initial_status = detail.status.borrow().clone();
        detail.apply(&initial_status);
        detail
    }

    pub(super) fn page(&self) -> &adw::NavigationPage {
        &self.page
    }

    /// The label pen, on the header of an account that is not the provider's default.
    ///
    /// The default account is the provider's structural first one; the daemon will not
    /// move it, so the pen stays off its page. A confirmed rename retires this page — it
    /// is keyed by an identity the rename just replaced — and the dialog returns to the
    /// list when the daemon announces the old id's removal. Nothing here chases the new
    /// id: the user is where the list is, and the new account's row is one click away.
    fn wire_rename(self: &Rc<Self>) {
        let status = self.status.borrow();
        if status.account == DEFAULT_ACCOUNT {
            return;
        }
        let provider = status.provider.clone();
        let account = status.account.clone();
        let label = status
            .account_label
            .clone()
            .unwrap_or_else(|| account.clone());
        drop(status);

        let rename = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Rename account")
            .valign(gtk::Align::Center)
            .build();
        rename.connect_clicked({
            let weak = Rc::downgrade(self);
            let label = label.clone();
            move |_| {
                let Some(detail) = weak.upgrade() else {
                    return;
                };
                let dialog = detail.dialog.clone();
                let proxy = detail.proxy.clone();
                let provider = provider.clone();
                let account = account.clone();
                let label = label.clone();
                glib::spawn_future_local(async move {
                    let Some(slug) =
                        name_dialog(&dialog, "Rename account", &label, "Rename", Some(&account))
                            .await
                    else {
                        return;
                    };
                    // Success needs no action here: the daemon publishes the old id's
                    // removal and the new id's arrival, and the page keyed by the old id
                    // is retired by that, not by anything it does to itself.
                    if let Err(error) = proxy.rename_account(&provider, &account, &slug).await {
                        detail.toast(&reason(&error));
                    }
                });
            }
        });
        self.header.pack_end(&rename);
    }

    pub(super) fn matches(&self, provider: &str, account: &str) -> bool {
        let status = self.status.borrow();
        status.provider == provider && status.account == account
    }

    /// Updates daemon-owned state only. Entry text and the option widgets themselves stay
    /// in place, so a poll cannot erase a key being typed.
    pub(super) fn apply(self: &Rc<Self>, status: &ProviderStatus) {
        *self.status.borrow_mut() = status.clone();
        // A preferences group parses its description as Pango markup and offers no
        // switch to turn that off, so the daemon's words — which can carry the same
        // `ai&` the titles do — are escaped instead.
        let description = glib::markup_escape_text(&describe(&self.definition, status));
        self.authentication
            .set_description(Some(description.as_str()));

        let stored = status.has_credential == Some(true);
        if let Some(rows) = self.key.borrow().as_ref() {
            rows.entry.set_title(if stored {
                "Replace the stored key"
            } else {
                "API key"
            });
            rows.save.set_label(if stored { "Replace" } else { "Save" });
            rows.remove.set_visible(stored);
        }
        if let Some(sign_in) = self.sign_in.borrow().as_ref()
            && !self.waiting.get()
        {
            sign_in
                .row
                .set_subtitle(&model::connection_text(&self.definition, status));
            sign_in
                .button
                .set_label(if stored { "Sign out" } else { "Sign in…" });
            sign_in.button.set_sensitive(true);
            sign_in.stack.set_visible_child_name(BUTTON);
        }
        if let Some(external) = self.external.borrow().as_ref() {
            external.set_title(&model::connection_text(&self.definition, status));
        }
        let rows = self.options.borrow();
        for option in &status.options {
            if let Some(row) = rows.get(&option.name) {
                row.apply_authoritative(&option.value);
            }
        }
        drop(rows);
        self.apply_local(status);
        self.apply_notifications(status);
        if let Some(browser) = self.browser_auth.borrow().as_ref() {
            browser.apply_selection(status.auth_selection.as_ref());
        }
    }

    /// Puts the pill and the two halves where the daemon says the account is.
    ///
    /// The half in force is the daemon's answer rather than the one last looked at: an
    /// account whose local login has just appeared, or whose Tidemark login has just been
    /// signed out of, has moved, and a dialog still showing the other half would be
    /// describing a credential nothing is using.
    fn apply_local(self: &Rc<Self>, status: &ProviderStatus) {
        let Some(source) = model::auth_source(&self.definition, status) else {
            return;
        };
        if let Some(choice) = self.choice.borrow().as_ref() {
            choice.apply_authoritative(source);
        }
        if let Some(local) = self.local.borrow().as_ref() {
            match model::external_presence_text(status) {
                Some(text) => {
                    // Coloured rather than only worded: this is the one line that says
                    // whether the half being looked at can work at all, and it sits at the
                    // end of a row whose title and subtitle are both black.
                    let colour = if status.external_present == Some(true) {
                        "success"
                    } else {
                        "warning"
                    };
                    local.presence.set_label(text);
                    local.presence.set_css_classes(&[colour]);
                    local.presence.set_visible(true);
                }
                None => local.presence.set_visible(false),
            }
        }
        self.show_source(source);
    }

    /// Shows one half and hides the other. A login in progress belongs to the Tidemark
    /// half, so the pill is held still until it finishes; see [`ProviderDetail::set_waiting`].
    fn show_source(&self, source: AuthSource) {
        if let Some(sign_in) = self.sign_in.borrow().as_ref() {
            sign_in.row.set_visible(source == AuthSource::Tidemark);
        }
        if let Some(local) = self.local.borrow().as_ref() {
            local.set_visible(source == AuthSource::Cli);
        }
    }

    /// Redraws the notification switches for the windows the account currently reports.
    ///
    /// The window set is whatever the last reading contained and can change between polls,
    /// so the rows are rebuilt when it does — and left alone when it has not, because
    /// replacing a row under a pointer that is on it loses the click.
    fn apply_notifications(self: &Rc<Self>, status: &ProviderStatus) {
        let wanted = model::notification_rows(status);
        self.notifications.set_visible(!wanted.is_empty());

        let same_windows = {
            let held = self.notify_rows.borrow();
            held.len() == wanted.len()
                && held
                    .iter()
                    .zip(&wanted)
                    .all(|(held, wanted)| held.key == wanted.key)
        };
        if !same_windows {
            for row in self.notify_rows.borrow_mut().drain(..) {
                self.notifications.remove(&row.row);
            }
            let rebuilt: Vec<Rc<NotifyRow>> = wanted
                .iter()
                .map(|wanted| self.build_notify_row(&wanted.key, &wanted.title, wanted.enabled))
                .collect();
            for row in &rebuilt {
                self.notifications.add(&row.row);
            }
            *self.notify_rows.borrow_mut() = rebuilt;
            return;
        }

        let held = self.notify_rows.borrow();
        for (held, wanted) in held.iter().zip(&wanted) {
            held.apply_authoritative(wanted.enabled);
        }
    }

    fn build_notify_row(self: &Rc<Self>, key: &str, title: &str, enabled: bool) -> Rc<NotifyRow> {
        // Titled by the window, not by its key: the key is what the daemon is told, and
        // showing it here would put an identifier in front of somebody who has the card's
        // own name for the same thing two clicks away. The window's name is the daemon's
        // words too, so markup stays off.
        let row = adw::SwitchRow::builder()
            .title(title)
            .active(enabled)
            .use_markup(false)
            .build();
        let notify_row = Rc::new(NotifyRow {
            key: key.to_owned(),
            row,
            state: SwitchState::new(enabled),
        });

        let watched = Rc::clone(&notify_row);
        notify_row.row.connect_active_notify({
            let weak = Rc::downgrade(self);
            move |row| {
                let Some(enabled) = watched.state.toggled(row.is_active()) else {
                    return;
                };
                let Some(detail) = weak.upgrade() else {
                    return;
                };
                let status = detail.status.borrow();
                let provider = status.provider.clone();
                let account = status.account.clone();
                drop(status);
                let window = watched.key.clone();
                let watched = Rc::clone(&watched);
                glib::spawn_future_local(async move {
                    if let Err(error) = detail
                        .proxy
                        .set_window_notify(&provider, &account, &window, enabled)
                        .await
                    {
                        watched.rollback();
                        detail.toast(&reason(&error));
                    }
                });
            }
        });
        notify_row
    }

    fn build_authentication(self: &Rc<Self>) {
        if self.definition.browser_auth.is_some() {
            self.build_browser_auth();
            return;
        }
        match self.definition.credential_kind() {
            Some(CredentialKind::Key) => self.build_key_rows(),
            // A provider whose credential can also come from a CLI on this machine gets
            // both halves and a pill to pick one. It is the same group either way: the two
            // are alternatives, not a control and its fallback, and drawing them as two
            // groups would suggest a login could be on both at once. The pill is added
            // first because the group stacks its rows in the order they arrive, and the
            // control that decides which rows below it mean anything belongs above them.
            Some(CredentialKind::OAuth) => {
                let external = self.definition.external.clone();
                if let Some(external) = &external {
                    self.build_source_choice(external);
                }
                self.build_sign_in_row();
                if let Some(external) = &external {
                    self.build_local_rows(external);
                }
            }
            // A real credential-free service exposes no authentication controls.
            Some(CredentialKind::None) => {
                self.authentication.set_visible(false);
            }
            Some(CredentialKind::External) | None => {
                let status = self.status.borrow();
                // The row's stay-off-markup rule: the title is the daemon's connection
                // words, which are data, never markup.
                let row = adw::ActionRow::builder()
                    .title(model::connection_text(&self.definition, &status))
                    .use_markup(false)
                    .build();
                self.authentication.add(&row);
                *self.external.borrow_mut() = Some(row);
            }
        }
    }

    fn build_key_rows(self: &Rc<Self>) {
        let entry = adw::PasswordEntryRow::builder().title("API key").build();
        let save = gtk::Button::builder()
            .label("Save")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .css_classes(["suggested-action"])
            .build();
        entry.add_suffix(&save);
        entry.connect_changed({
            let save = save.clone();
            move |entry| save.set_sensitive(!entry.text().trim().is_empty())
        });

        let store = {
            let weak = Rc::downgrade(self);
            let entry = entry.clone();
            move || {
                let key = entry.text().trim().to_owned();
                if key.is_empty() {
                    return;
                }
                entry.set_text("");
                let Some(detail) = weak.upgrade() else {
                    return;
                };
                let status = detail.status.borrow();
                let provider = status.provider.clone();
                let account = status.account.clone();
                drop(status);
                glib::spawn_future_local(async move {
                    match detail.proxy.set_key(&provider, &account, &key).await {
                        Ok(()) => detail.toast("Key saved. Checking the account…"),
                        Err(error) => detail.toast(&reason(&error)),
                    }
                });
            }
        };
        save.connect_clicked({
            let store = store.clone();
            move |_| store()
        });
        entry.connect_entry_activated(move |_| store());
        self.authentication.add(&entry);

        let remove = adw::ActionRow::builder()
            .title("Stored key")
            .subtitle("Removing it leaves the account with no credential.")
            .build();
        let button = gtk::Button::builder()
            .label("Remove")
            .valign(gtk::Align::Center)
            .css_classes(["destructive-action"])
            .build();
        button.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                let Some(detail) = weak.upgrade() else {
                    return;
                };
                let status = detail.status.borrow();
                let provider = status.provider.clone();
                let account = status.account.clone();
                drop(status);
                glib::spawn_future_local(async move {
                    match detail.proxy.sign_out(&provider, &account).await {
                        Ok(()) => detail.toast("Key removed."),
                        Err(error) => detail.toast(&reason(&error)),
                    }
                });
            }
        });
        remove.add_suffix(&button);
        self.authentication.add(&remove);
        *self.key.borrow_mut() = Some(KeyRows {
            entry,
            save,
            remove,
        });
    }

    fn build_sign_in_row(self: &Rc<Self>) {
        // Markup off for the row as a whole: the subtitle `apply` sets later carries the
        // daemon's connection words, which are data, never markup.
        let row = adw::ActionRow::builder()
            .title("OAuth login")
            .use_markup(false)
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
            let weak = Rc::downgrade(self);
            let url = Rc::clone(&url);
            move |button| {
                let address = url.borrow().clone();
                if address.is_empty() {
                    return;
                }
                button.clipboard().set_text(&address);
                if let Some(detail) = weak.upgrade() {
                    detail.toast("Login address copied. Open it in a browser.");
                }
            }
        });

        let stack = gtk::Stack::builder()
            .valign(gtk::Align::Center)
            .hhomogeneous(false)
            .build();
        stack.add_named(&button, Some(BUTTON));
        stack.add_named(&waiting, Some(WAITING));
        row.add_suffix(&stack);
        self.authentication.add(&row);

        button.connect_clicked({
            let weak = Rc::downgrade(self);
            move |button| {
                let Some(detail) = weak.upgrade() else {
                    return;
                };
                let signed_in = button.label().is_some_and(|label| label == "Sign out");
                let status = detail.status.borrow();
                let provider = status.provider.clone();
                let account = status.account.clone();
                drop(status);
                if signed_in {
                    glib::spawn_future_local(async move {
                        match detail.proxy.sign_out(&provider, &account).await {
                            Ok(()) => detail.toast("Signed out of Tidemark's account."),
                            Err(error) => detail.toast(&reason(&error)),
                        }
                    });
                } else {
                    glib::spawn_future_local(async move {
                        detail.sign_in(provider, account).await;
                    });
                }
            }
        });
        cancel.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                let Some(detail) = weak.upgrade() else {
                    return;
                };
                let status = detail.status.borrow();
                let provider = status.provider.clone();
                let account = status.account.clone();
                drop(status);
                glib::spawn_future_local(async move {
                    let _ = detail.proxy.cancel_login(&provider, &account).await;
                });
            }
        });

        *self.sign_in.borrow_mut() = Some(SignInRow {
            row,
            button,
            stack,
            url,
        });
    }

    /// The pill in the group's header that picks between the two credentials.
    ///
    /// Built from the setting the daemon named rather than from anything known here, so
    /// the halves are labelled in the provider's own words — `Claude Code login`, not a
    /// slug this crate would have to be taught. A choice this build has no screen for is
    /// refused outright: half a pill is worse than none, because the half that is drawn
    /// looks like the whole choice.
    fn build_source_choice(self: &Rc<Self>, external: &ExternalLogin) {
        let Some(option) = self
            .definition
            .options
            .iter()
            .find(|option| option.name == external.option)
        else {
            return;
        };
        let toggles: Vec<(AuthSource, &str)> = option
            .choices
            .iter()
            .filter_map(|choice| {
                Some((
                    AuthSource::from_value(&choice.value)?,
                    choice.title.as_str(),
                ))
            })
            .collect();
        if toggles.len() != option.choices.len() || toggles.len() < 2 {
            return;
        }

        // Full width, in a row of its own at the top of the group, rather than tucked into
        // the group's header. The two labels are the providers' own names for the two
        // credentials — `Claude Code login` is not shortenable without becoming a guess —
        // and in a header suffix they share what the heading and its description leave
        // over, which was enough to ellipsize both of them.
        let group = adw::ToggleGroup::builder()
            .hexpand(true)
            .homogeneous(true)
            .build();
        for (source, title) in &toggles {
            group.add(
                adw::Toggle::builder()
                    .name(source.as_value())
                    .label(*title)
                    .build(),
            );
        }
        let row = adw::PreferencesRow::builder()
            .activatable(false)
            .child(&group)
            .build();
        row.add_css_class("credential-choice");
        self.authentication.add(&row);

        group.connect_active_name_notify({
            let weak = Rc::downgrade(self);
            move |_| {
                let Some(detail) = weak.upgrade() else {
                    return;
                };
                let asked = detail.choice.borrow().as_ref().and_then(|choice| {
                    choice
                        .clicked()
                        .map(|source| (source, choice.option.clone()))
                });
                let Some((chosen, name)) = asked else {
                    return;
                };
                // Moved before the write lands: the pill is the one control here whose
                // two positions are two screens, and leaving the old screen under a
                // pill that has already moved reads as the click having missed.
                detail.show_source(chosen);
                let status = detail.status.borrow();
                let provider = status.provider.clone();
                let account = status.account.clone();
                drop(status);
                let value = chosen.as_value().to_owned();
                glib::spawn_future_local(async move {
                    if let Err(error) = detail
                        .proxy
                        .set_option(&provider, &account, &name, &value)
                        .await
                    {
                        let restored = detail.choice.borrow().as_ref().map(SourceChoice::rollback);
                        if let Some(restored) = restored {
                            detail.show_source(restored);
                        }
                        detail.toast(&reason(&error));
                    }
                });
            }
        });

        *self.choice.borrow_mut() = Some(SourceChoice {
            option: external.option.clone(),
            group,
            authoritative: Cell::new(AuthSource::Tidemark),
            suppress: Cell::new(false),
        });
    }

    /// The tabs and source rows for picking which login on this machine this account
    /// reads.
    ///
    /// The tabs come from the daemon's selector and the rows from its latest inspection;
    /// a click anywhere lands as one Select that the daemon validates before storing any
    /// of it. First inspection starts here, so the page never opens as if nothing were
    /// being checked.
    fn build_browser_auth(self: &Rc<Self>) {
        let Some(selector) = self.definition.browser_auth.clone() else {
            return;
        };
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Check again")
            .valign(gtk::Align::Center)
            .css_classes(["circular"])
            .build();
        refresh.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(detail) = weak.upgrade() {
                    detail.open_browser_auth();
                }
            }
        });
        self.header.pack_end(&refresh);

        let weak = Rc::downgrade(self);
        let on_choose: Rc<dyn Fn(AuthSelection)> = Rc::new(move |selection| {
            if let Some(detail) = weak.upgrade() {
                let proxy = detail.proxy.clone();
                let (provider, account) = {
                    let status = detail.status.borrow();
                    (status.provider.clone(), status.account.clone())
                };
                glib::spawn_future_local(async move {
                    // The daemon validates inside the write, so a refused source leaves the
                    // previous one in force; there is nothing local to roll back, only the
                    // reason to say out loud.
                    if let Err(error) = proxy
                        .select_auth_source(&provider, &account, selection)
                        .await
                    {
                        detail.toast(&reason(&error));
                        return;
                    }
                    // Asking the daemon to publish now makes the accepted selection's In-use
                    // mark arrive with its status rather than at the next scheduled poll.
                    if let Err(error) = proxy.refresh(&provider).await {
                        detail.toast(&reason(&error));
                    }
                });
            }
        });
        let published = self.status.borrow().auth_selection.clone();
        let rows = BrowserAuth::new(&selector, published.as_ref(), on_choose);
        rows.attach(&self.authentication);
        *self.browser_auth.borrow_mut() = Some(rows);

        // Construction counts as opening: the report is what turns Checking… into rows.
        self.open_browser_auth();
    }

    /// Inspects this account's local sources again.
    ///
    /// The halves go to their checking note first, so a slow daemon reads as busy rather
    /// than broken; a failure then restores whatever was on screen instead of an empty
    /// page, because refusing to answer is not evidence about any source.
    fn open_browser_auth(self: &Rc<Self>) {
        {
            let guard = self.browser_auth.borrow();
            let Some(rows) = guard.as_ref() else {
                return;
            };
            rows.begin_loading();
        }
        let (provider, account) = {
            let status = self.status.borrow();
            (status.provider.clone(), status.account.clone())
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let report = match weak.upgrade() {
                Some(detail) => detail.proxy.get_auth_sources(&provider, &account).await,
                None => return,
            };
            let Some(detail) = weak.upgrade() else {
                return;
            };
            match report {
                Ok(report) => {
                    if let Some(rows) = detail.browser_auth.borrow().as_ref() {
                        rows.apply_report(report);
                    }
                }
                Err(error) => {
                    if let Some(rows) = detail.browser_auth.borrow().as_ref() {
                        rows.recover();
                    }
                    detail.toast(&reason(&error));
                }
            }
        });
    }

    /// The half that reads a login another program on this machine holds.
    ///
    /// Three things, in the order somebody with a problem needs them: whether the login is
    /// there and where Tidemark looked, what to run if it is not, and — for the two
    /// providers this is true of — that Tidemark refreshes the credential in place and
    /// writes the new token back into a file it does not own. The last of those is ADR
    /// 0001, and it is stated in the open rather than behind a disclosure: a program that
    /// edits another program's credentials has to say so where the choice is made.
    fn build_local_rows(self: &Rc<Self>, external: &ExternalLogin) {
        // Markup off throughout: every string here is the daemon's, and a path is data.
        let found = adw::ActionRow::builder()
            .title(&external.label)
            .subtitle(&external.location)
            .use_markup(false)
            .visible(false)
            .build();
        let presence = gtk::Label::builder()
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        let recheck = gtk::Button::builder()
            .label("Check again")
            .valign(gtk::Align::Center)
            .build();
        recheck.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                let Some(detail) = weak.upgrade() else {
                    return;
                };
                let provider = detail.status.borrow().provider.clone();
                glib::spawn_future_local(async move {
                    if let Err(error) = detail.proxy.refresh(&provider).await {
                        detail.toast(&reason(&error));
                    }
                });
            }
        });
        let suffix = gtk::Box::builder().spacing(8).build();
        suffix.append(&presence);
        suffix.append(&recheck);
        found.add_suffix(&suffix);
        self.authentication.add(&found);

        let command = (!external.command.is_empty()).then(|| {
            let row = adw::ActionRow::builder()
                .title("Sign in with the CLI")
                .subtitle(&external.command)
                .use_markup(false)
                .visible(false)
                .build();
            let copy = gtk::Button::builder()
                .label("Copy")
                .valign(gtk::Align::Center)
                .build();
            copy.connect_clicked({
                let weak = Rc::downgrade(self);
                let command = external.command.clone();
                move |button| {
                    button.clipboard().set_text(&command);
                    if let Some(detail) = weak.upgrade() {
                        detail.toast("Command copied. Run it in a terminal.");
                    }
                }
            });
            row.add_suffix(&copy);
            self.authentication.add(&row);
            row
        });

        let note = model::write_back_text(external).map(|text| {
            let label = caption(&text);
            label.set_visible(false);
            self.authentication.add(&label);
            label
        });

        *self.local.borrow_mut() = Some(LocalRows {
            found,
            presence,
            command,
            note,
        });
    }

    /// The provider's own settings — every one except the credential choice, which the
    /// authentication group above draws as a pill. Left in the list it would appear twice,
    /// and the second one would be a menu offering the same two values under a heading
    /// that says nothing about which credential is in use.
    fn build_options(self: &Rc<Self>, group: &adw::PreferencesGroup) {
        let status = self.status.borrow();
        let declared = if status.options.is_empty() {
            &self.definition.options
        } else {
            &status.options
        };
        // Two exclusions, one per control that owns its setting: the credential pill
        // draws the OAuth source choice, and the authentication tabs own every local
        // source identifier. What survives is genuinely a menu.
        let auth_option = self.definition.auth_option();
        let pill_excluded: Vec<&ProviderOption> = declared
            .iter()
            .filter(|option| Some(option.name.as_str()) != auth_option)
            .collect();
        let options = model::settings_options(pill_excluded, &self.definition);
        group.set_visible(!options.is_empty());
        for option in options {
            let row = self.build_option_row(option);
            group.add(&row.row);
            self.options
                .borrow_mut()
                .insert(option.name.clone(), Rc::clone(&row));
            if let Some(description) = &option.description {
                group.add(&caption(description));
            }
        }
    }

    fn build_option_row(self: &Rc<Self>, option: &ProviderOption) -> Rc<OptionRow> {
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
            .use_subtitle(true)
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
        let option_row = Rc::new(OptionRow {
            row,
            selection: OptionSelection::new(values, option.value.clone(), selected),
        });
        let watched = Rc::clone(&option_row);
        option_row.row.connect_selected_notify({
            let weak = Rc::downgrade(self);
            let name = option.name.clone();
            move |row| {
                let Some(chosen) = watched.selection.selection_changed(row.selected()) else {
                    return;
                };
                let Some(detail) = weak.upgrade() else {
                    return;
                };
                let status = detail.status.borrow();
                let provider = status.provider.clone();
                let account = status.account.clone();
                drop(status);
                let name = name.clone();
                let option_row = Rc::clone(&watched);
                glib::spawn_future_local(async move {
                    if let Err(error) = detail
                        .proxy
                        .set_option(&provider, &account, &name, &chosen)
                        .await
                    {
                        option_row.rollback();
                        detail.toast(&reason(&error));
                    }
                });
            }
        });
        option_row
    }

    async fn sign_in(self: Rc<Self>, provider: String, account: String) {
        let url = match self.proxy.begin_login(&provider, &account).await {
            Ok(url) => url,
            Err(error) => {
                self.toast(&reason(&error));
                return;
            }
        };
        match after_begin_action(self.set_waiting(&provider, &account, Some(&url))) {
            AfterBeginAction::OpenBrowserAndAwait => {}
            AfterBeginAction::CancelLogin => {
                let _ = self.proxy.cancel_login(&provider, &account).await;
                return;
            }
        }

        // GTK's URI launcher can report success on Windows without dispatching the
        // URL. `webbrowser` uses the Windows shell's default-browser handler instead.
        #[cfg(windows)]
        let launch = webbrowser::open(&url);
        #[cfg(unix)]
        let launch = gtk::UriLauncher::new(&url)
            .launch_future(self.dialog.root().and_downcast_ref::<gtk::Window>())
            .await;
        if let Err(error) = launch {
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

    fn set_waiting(self: &Rc<Self>, provider: &str, account: &str, url: Option<&str>) -> bool {
        if !(self.on_waiting)(provider.into(), account.into(), url.is_some()) {
            return false;
        }
        self.waiting.set(url.is_some());
        // A login in progress belongs to the Tidemark half and holds a fixed loopback
        // port. Letting the pill move under it would hide the Cancel button while the
        // listener is still bound, which is the one way to leave that port held.
        if let Some(choice) = self.choice.borrow().as_ref() {
            choice.group.set_sensitive(url.is_none());
        }
        let sign_in_rows = self.sign_in.borrow();
        let Some(sign_in) = sign_in_rows.as_ref() else {
            return true;
        };
        match url {
            Some(url) => {
                *sign_in.url.borrow_mut() = url.into();
                sign_in.row.set_subtitle("Waiting for your browser…");
                sign_in.stack.set_visible_child_name(WAITING);
            }
            None => {
                sign_in.url.borrow_mut().clear();
                sign_in.stack.set_visible_child_name(BUTTON);
                let status = self.status.borrow().clone();
                drop(sign_in_rows);
                self.apply(&status);
            }
        }
        true
    }

    fn toast(&self, message: &str) {
        self.dialog.add_toast(adw::Toast::new(message));
    }
}

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

/// The sentence under the group's heading: what is wrong, and where the credential comes
/// from.
///
/// The second half is dropped for a provider whose two credentials are drawn as a pill.
/// The hint for those reads "sign in through Tidemark, or read Claude Code's own login",
/// which is the pill spelled out in prose directly above the pill — and the width it took
/// was width the two labels needed.
fn describe(definition: &ProviderDefinition, status: &ProviderStatus) -> String {
    let hint = if definition.external.is_some() {
        ""
    } else {
        definition.credential_hint.as_str()
    };
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
        _ => hint.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use tidemark_types::{AccountId, ProviderId, ProviderState};

    use super::*;

    fn definition(kind: CredentialKind) -> ProviderDefinition {
        ProviderDefinition {
            provider: "zai".into(),
            title: "Z.ai".into(),
            credential: kind.as_wire().into(),
            credential_hint: "Z.ai dashboard → API keys.".into(),
            external: None,
            browser_auth: None,
            options: Vec::new(),
        }
    }

    fn status() -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default())
    }

    #[test]
    fn a_healthy_account_is_described_by_where_its_credential_comes_from() {
        let definition = definition(CredentialKind::Key);
        let mut healthy = status();
        healthy.set_state(ProviderState::Ok, None);
        assert_eq!(describe(&definition, &healthy), definition.credential_hint);
    }

    #[test]
    fn an_account_the_user_must_fix_leads_with_what_is_wrong() {
        let definition = definition(CredentialKind::Key);
        let mut rejected = status();
        rejected.set_state(
            ProviderState::CredentialRejected,
            Some("the credential was rejected (HTTP 401)".into()),
        );
        assert_eq!(
            describe(&definition, &rejected),
            "the credential was rejected (HTTP 401) — Z.ai dashboard → API keys."
        );
    }

    #[test]
    fn an_account_that_is_merely_waiting_does_not_shout_about_it() {
        let definition = definition(CredentialKind::Key);
        assert_eq!(describe(&definition, &status()), definition.credential_hint);
    }

    #[test]
    fn a_state_this_build_does_not_know_is_still_reported() {
        let definition = definition(CredentialKind::OAuth);
        let mut unknown = status();
        unknown.state = "quota-frozen".into();
        assert!(
            describe(&definition, &unknown).starts_with("quota-frozen"),
            "{}",
            describe(&definition, &unknown)
        );
    }

    #[test]
    fn an_authoritative_option_update_changes_the_display_without_requesting_a_write() {
        let selection = OptionSelection::new(
            vec!["global".into(), "bigmodel-cn".into()],
            "global".into(),
            0,
        );
        let requested = RefCell::new(None);

        selection.apply_authoritative("bigmodel-cn", |index| {
            *requested.borrow_mut() = selection.selection_changed(index);
        });

        assert_eq!(selection.displayed_value(), Some("bigmodel-cn"));
        assert_eq!(*requested.borrow(), None);
    }

    #[test]
    fn a_rejected_option_write_restores_the_last_authoritative_selection() {
        let selection = OptionSelection::new(
            vec!["global".into(), "bigmodel-cn".into()],
            "global".into(),
            0,
        );
        assert_eq!(
            selection.selection_changed(1),
            Some("bigmodel-cn".to_owned())
        );
        assert_eq!(selection.displayed_value(), Some("bigmodel-cn"));
        let requested = RefCell::new(None);

        selection.rollback(|index| {
            *requested.borrow_mut() = selection.selection_changed(index);
        });

        assert_eq!(selection.displayed_value(), Some("global"));
        assert_eq!(*requested.borrow(), None);
    }

    #[test]
    fn a_quick_correction_back_to_the_authoritative_value_still_requests_a_write() {
        let selection = OptionSelection::new(
            vec!["global".into(), "bigmodel-cn".into()],
            "global".into(),
            0,
        );

        assert_eq!(
            selection.selection_changed(1),
            Some("bigmodel-cn".to_owned())
        );
        assert_eq!(selection.selection_changed(0), Some("global".to_owned()));
    }

    #[test]
    fn flicking_a_switch_requests_the_write_it_shows() {
        let state = SwitchState::new(false);
        assert_eq!(state.toggled(true), Some(true));
        assert_eq!(state.toggled(false), Some(false));
    }

    #[test]
    fn an_authoritative_switch_update_changes_the_display_without_requesting_a_write() {
        let state = SwitchState::new(false);
        let requested = RefCell::new(None);

        state.apply_authoritative(true, |active| {
            *requested.borrow_mut() = state.toggled(active);
        });

        assert!(state.displayed());
        assert_eq!(*requested.borrow(), None);
    }

    #[test]
    fn a_rejected_switch_write_restores_the_last_authoritative_state() {
        let state = SwitchState::new(false);
        assert_eq!(state.toggled(true), Some(true));
        let requested = RefCell::new(None);

        state.rollback(|active| {
            *requested.borrow_mut() = state.toggled(active);
        });

        assert!(!state.displayed());
        assert_eq!(*requested.borrow(), None);
    }
}
