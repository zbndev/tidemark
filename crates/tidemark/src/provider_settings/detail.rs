use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tidemark_types::{CredentialKind, ProviderDefinition, ProviderOption, ProviderStatus, Remedy};

use super::{model, reason};
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
    authentication: adw::PreferencesGroup,
    key: RefCell<Option<KeyRows>>,
    sign_in: RefCell<Option<SignInRow>>,
    external: RefCell<Option<adw::ActionRow>>,
    options: RefCell<BTreeMap<String, Rc<OptionRow>>>,
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
        (!self.suppress.get() && *self.authoritative.borrow() != *chosen).then(|| chosen.clone())
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
        let preferences = adw::PreferencesPage::new();
        preferences.add(&authentication);
        preferences.add(&options);
        let toolbar = adw::ToolbarView::builder().content(&preferences).build();
        toolbar.add_top_bar(&header);
        let page = adw::NavigationPage::new(&toolbar, &definition.title);

        let detail = Rc::new(Self {
            dialog: dialog.clone(),
            proxy,
            definition,
            status: RefCell::new(status),
            page,
            authentication,
            key: RefCell::new(None),
            sign_in: RefCell::new(None),
            external: RefCell::new(None),
            options: RefCell::new(BTreeMap::new()),
            waiting: Cell::new(false),
            on_waiting,
        });
        detail.build_authentication();
        detail.build_options(&options);
        let initial_status = detail.status.borrow().clone();
        detail.apply(&initial_status);
        detail
    }

    pub(super) fn page(&self) -> &adw::NavigationPage {
        &self.page
    }

    pub(super) fn matches(&self, provider: &str, account: &str) -> bool {
        let status = self.status.borrow();
        status.provider == provider && status.account == account
    }

    /// Updates daemon-owned state only. Entry text and the option widgets themselves stay
    /// in place, so a poll cannot erase a key being typed.
    pub(super) fn apply(&self, status: &ProviderStatus) {
        *self.status.borrow_mut() = status.clone();
        self.authentication
            .set_description(Some(&describe(&self.definition, status)));

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
    }

    fn build_authentication(self: &Rc<Self>) {
        match self.definition.credential_kind() {
            Some(CredentialKind::Key) => self.build_key_rows(),
            Some(CredentialKind::OAuth) => self.build_sign_in_row(),
            Some(CredentialKind::External) | None => {
                let status = self.status.borrow();
                let row = adw::ActionRow::builder()
                    .title(model::connection_text(&self.definition, &status))
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

    fn build_options(self: &Rc<Self>, group: &adw::PreferencesGroup) {
        let status = self.status.borrow();
        let options = if status.options.is_empty() {
            &self.definition.options
        } else {
            &status.options
        };
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

    fn set_waiting(&self, provider: &str, account: &str, url: Option<&str>) -> bool {
        if !(self.on_waiting)(provider.into(), account.into(), url.is_some()) {
            return false;
        }
        self.waiting.set(url.is_some());
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

fn describe(definition: &ProviderDefinition, status: &ProviderStatus) -> String {
    let hint = &definition.credential_hint;
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
        _ => hint.clone(),
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
            external_fallback: None,
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
}
