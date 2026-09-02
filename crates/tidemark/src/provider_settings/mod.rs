//! Navigable provider configuration: configured list, add picker, and stable details.

mod browser_auth;
mod detail;
mod list;
pub mod model;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;
use tidemark_types::{
    AccountId, CredentialKind, ProviderDefinition, ProviderId, ProviderStatus,
    account_slug_suggestion, valid_account_slug,
};

use self::detail::ProviderDetail;
use self::list::{ConfiguredList, Picker};
use crate::bus::DaemonProxy;

/// The account a provider's structural first add holds. The daemon will not rename it,
/// so the label pen stays off its page.
pub(super) const DEFAULT_ACCOUNT: &str = "default";

/// OAuth attempts which the open dialog is responsible for cancelling on close.
#[derive(Debug, Default)]
struct PendingLogins {
    identities: RefCell<HashSet<(String, String)>>,
    closed: Cell<bool>,
}

impl PendingLogins {
    fn insert(&self, provider: &str, account: &str) -> bool {
        if self.closed.get() {
            return false;
        }
        self.identities
            .borrow_mut()
            .insert((provider.into(), account.into()));
        true
    }

    fn remove(&self, provider: &str, account: &str) {
        self.identities
            .borrow_mut()
            .remove(&(provider.into(), account.into()));
    }

    fn contains(&self, provider: &str, account: &str) -> bool {
        self.identities
            .borrow()
            .contains(&(provider.into(), account.into()))
    }

    fn take_all(&self) -> Vec<(String, String)> {
        self.identities.borrow_mut().drain().collect()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug)]
struct CachedDetail<T> {
    provider: String,
    account: String,
    value: T,
}

#[derive(Debug)]
struct DetailCache<T>(Vec<CachedDetail<T>>);

impl<T> Default for DetailCache<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> DetailCache<T> {
    fn get(&self, provider: &str, account: &str) -> Option<&T> {
        self.0
            .iter()
            .find(|detail| detail.provider == provider && detail.account == account)
            .map(|detail| &detail.value)
    }

    fn insert(&mut self, provider: &str, account: &str, value: T) {
        self.remove(provider, account);
        self.0.push(CachedDetail {
            provider: provider.into(),
            account: account.into(),
            value,
        });
    }

    fn remove(&mut self, provider: &str, account: &str) {
        self.0
            .retain(|detail| detail.provider != provider || detail.account != account);
    }

    fn retain(&mut self, mut keep: impl FnMut(&str, &str) -> bool) {
        self.0
            .retain(|detail| keep(&detail.provider, &detail.account));
    }

    fn values(&self) -> impl Iterator<Item = &T> {
        self.0.iter().map(|detail| &detail.value)
    }
}

#[derive(Debug, Default)]
struct ActiveDetail(Option<(String, String)>);

impl ActiveDetail {
    fn show(&mut self, provider: &str, account: &str) {
        self.0 = Some((provider.into(), account.into()));
    }

    fn hide(&mut self, provider: &str, account: &str) {
        if self
            .0
            .as_ref()
            .is_some_and(|identity| identity.0 == provider && identity.1 == account)
        {
            self.0 = None;
        }
    }

    fn take_if_missing(&mut self, statuses: &[ProviderStatus]) -> bool {
        let missing = self.0.as_ref().is_some_and(|(provider, account)| {
            !statuses
                .iter()
                .any(|status| status.provider == *provider && status.account == *account)
        });
        if missing {
            self.0 = None;
        }
        missing
    }

    #[cfg(test)]
    fn identity(&self) -> Option<(&str, &str)> {
        self.0
            .as_ref()
            .map(|(provider, account)| (provider.as_str(), account.as_str()))
    }
}

fn remove_local_provider<T>(
    statuses: &mut Vec<ProviderStatus>,
    local_added: &mut HashSet<(String, String)>,
    details: &mut DetailCache<T>,
    provider: &str,
    account: &str,
) {
    local_added.remove(&(provider.into(), account.into()));
    statuses.retain(|status| status.provider != provider || status.account != account);
    details.remove(provider, account);
}

/// Owns every page for one open provider-settings dialog.
#[derive(Debug)]
pub struct ProviderSettings {
    dialog: adw::PreferencesDialog,
    proxy: DaemonProxy<'static>,
    definitions: RefCell<Vec<ProviderDefinition>>,
    statuses: RefCell<Vec<ProviderStatus>>,
    local_added: RefCell<HashSet<(String, String)>>,
    configured: ConfiguredList,
    picker: Rc<Picker>,
    details: RefCell<DetailCache<Rc<ProviderDetail>>>,
    active_detail: RefCell<ActiveDetail>,
    pending: Rc<PendingLogins>,
    self_weak: RefCell<Weak<ProviderSettings>>,
}

impl ProviderSettings {
    pub fn present(
        parent: &impl IsA<gtk::Widget>,
        proxy: DaemonProxy<'static>,
        definitions: &[ProviderDefinition],
        statuses: &[ProviderStatus],
        on_closed: impl Fn() + 'static,
    ) -> Rc<Self> {
        let dialog = adw::PreferencesDialog::builder()
            .title("Providers")
            .content_width(560)
            .content_height(680)
            .build();

        let controller: Rc<RefCell<Option<Weak<Self>>>> = Rc::new(RefCell::new(None));
        let configured = ConfiguredList::new({
            let controller = Rc::clone(&controller);
            Rc::new(move || {
                if let Some(settings) = controller.borrow().as_ref().and_then(Weak::upgrade) {
                    settings.open_picker();
                }
            })
        });
        let page = adw::PreferencesPage::new();
        page.add(&configured.group);
        dialog.add(&page);

        let picker = Picker::new({
            let controller = Rc::clone(&controller);
            Rc::new(move |provider| {
                if let Some(settings) = controller.borrow().as_ref().and_then(Weak::upgrade) {
                    settings.add_provider(provider);
                }
            })
        });
        let settings = Rc::new(Self {
            dialog: dialog.clone(),
            proxy,
            definitions: RefCell::new(Vec::new()),
            statuses: RefCell::new(Vec::new()),
            local_added: RefCell::new(HashSet::new()),
            configured,
            picker,
            details: RefCell::new(DetailCache::default()),
            active_detail: RefCell::new(ActiveDetail::default()),
            pending: Rc::new(PendingLogins::default()),
            self_weak: RefCell::new(Weak::new()),
        });
        *controller.borrow_mut() = Some(Rc::downgrade(&settings));
        *settings.self_weak.borrow_mut() = Rc::downgrade(&settings);

        settings.apply(definitions, statuses);
        dialog.connect_closed({
            let weak = Rc::downgrade(&settings);
            move |_| {
                if let Some(settings) = weak.upgrade() {
                    settings.cancel_pending_logins();
                    on_closed();
                }
            }
        });
        dialog.present(Some(parent));
        settings
    }

    /// Applies daemon-owned catalog/status state while keeping every existing detail
    /// widget alive. This is what protects text currently being typed into a secret row.
    pub fn apply(&self, definitions: &[ProviderDefinition], statuses: &[ProviderStatus]) {
        *self.definitions.borrow_mut() = definitions.to_vec();
        let merged = model::merge_local_additions(
            statuses,
            &self.statuses.borrow(),
            &self.local_added.borrow(),
        );
        self.local_added.borrow_mut().retain(|(provider, account)| {
            !statuses
                .iter()
                .any(|status| &status.provider == provider && &status.account == account)
        });
        let return_to_list = self.active_detail.borrow_mut().take_if_missing(&merged);
        *self.statuses.borrow_mut() = merged;
        self.details.borrow_mut().retain(|provider, account| {
            self.statuses
                .borrow()
                .iter()
                .any(|status| status.provider == provider && status.account == account)
        });
        if return_to_list {
            self.dialog.pop_subpage();
        }
        self.refresh_views();

        let statuses = self.statuses.borrow();
        for detail in self.details.borrow().values() {
            if let Some(status) = statuses
                .iter()
                .find(|status| detail.matches(&status.provider, &status.account))
            {
                detail.apply(status);
            }
        }
    }

    fn refresh_views(&self) {
        let definitions = self.definitions.borrow();
        let statuses = self.statuses.borrow();
        self.configured.apply(
            &definitions,
            &statuses,
            &|provider, account| self.pending.contains(provider, account),
            self.identity_callback(Self::open_detail),
            self.identity_callback(Self::confirm_removal),
            self.provider_callback(Self::add_account),
        );
        self.picker.apply(&definitions, &statuses);
    }

    fn identity_callback(
        &self,
        action: fn(&Rc<Self>, String, String),
    ) -> Rc<dyn Fn(String, String)> {
        let weak = self.self_weak.borrow().clone();
        Rc::new(move |provider, account| {
            if let Some(settings) = weak.upgrade() {
                action(&settings, provider, account);
            }
        })
    }

    fn provider_callback(&self, action: fn(&Rc<Self>, String)) -> Rc<dyn Fn(String)> {
        let weak = self.self_weak.borrow().clone();
        Rc::new(move |provider| {
            if let Some(settings) = weak.upgrade() {
                action(&settings, provider);
            }
        })
    }

    fn open_picker(&self) {
        self.picker
            .apply(&self.definitions.borrow(), &self.statuses.borrow());
        self.dialog.push_subpage(self.picker.page());
    }

    fn add_provider(self: &Rc<Self>, provider: String) {
        let settings = Rc::clone(self);
        glib::spawn_future_local(async move {
            if let Err(error) = settings.proxy.add_provider(&provider).await {
                settings.toast(&reason(&error));
                return;
            }

            let definition = settings
                .definitions
                .borrow()
                .iter()
                .find(|definition| definition.provider == provider)
                .cloned();
            let Some(definition) = definition else {
                return;
            };
            if !settings
                .statuses
                .borrow()
                .iter()
                .any(|status| status.provider == provider && status.account == DEFAULT_ACCOUNT)
            {
                settings
                    .local_added
                    .borrow_mut()
                    .insert((provider.clone(), DEFAULT_ACCOUNT.to_owned()));
                settings
                    .statuses
                    .borrow_mut()
                    .push(pending_status(&definition, DEFAULT_ACCOUNT));
            }
            settings.refresh_views();
            settings.dialog.pop_subpage();
            if opens_detail_after_add(&definition) {
                settings.open_detail(provider, AccountId::default().to_string());
            }
        });
    }

    /// The provider row's "+": asks for a name, has the daemon add the account it
    /// suggests, and lands on that account's page to give it a credential.
    fn add_account(self: &Rc<Self>, provider: String) {
        let definition = self
            .definitions
            .borrow()
            .iter()
            .find(|definition| definition.provider == provider)
            .cloned();
        let Some(definition) = definition else {
            return;
        };
        let settings = Rc::clone(self);
        glib::spawn_future_local(async move {
            let Some(slug) = name_dialog(
                &settings.dialog,
                &format!("New {} account", definition.title),
                "",
                "Add",
                None,
            )
            .await
            else {
                return;
            };
            if let Err(error) = settings.proxy.add_account(&provider, &slug).await {
                settings.toast(&reason(&error));
                return;
            }
            // The daemon persists the account before it answers, and publishes its first
            // status from its own task; until that lands, the account exists only as this
            // pending row, kept alive by the same machinery an added provider's is.
            if !settings
                .statuses
                .borrow()
                .iter()
                .any(|status| status.provider == provider && status.account == slug)
            {
                settings
                    .local_added
                    .borrow_mut()
                    .insert((provider.clone(), slug.clone()));
                settings
                    .statuses
                    .borrow_mut()
                    .push(pending_status(&definition, &slug));
            }
            settings.refresh_views();
            settings.open_detail(provider, slug);
        });
    }

    fn open_detail(self: &Rc<Self>, provider: String, account: String) {
        if let Some(existing) = self.details.borrow().get(&provider, &account).cloned() {
            self.active_detail.borrow_mut().show(&provider, &account);
            self.dialog.push_subpage(existing.page());
            return;
        }

        let definition = self
            .definitions
            .borrow()
            .iter()
            .find(|definition| definition.provider == provider)
            .cloned();
        let status = self
            .statuses
            .borrow()
            .iter()
            .find(|status| status.provider == provider && status.account == account)
            .cloned();
        let (Some(definition), Some(status)) = (definition, status) else {
            return;
        };
        let pending = Rc::clone(&self.pending);
        let detail = ProviderDetail::new(&self.dialog, self.proxy.clone(), definition, status, {
            let weak = self.self_weak.borrow().clone();
            Rc::new(move |provider, account, waiting| {
                let accepted = if waiting {
                    pending.insert(&provider, &account)
                } else {
                    pending.remove(&provider, &account);
                    true
                };
                if let Some(settings) = weak.upgrade() {
                    settings.refresh_views();
                }
                accepted
            })
        });
        detail.page().connect_hidden({
            let weak = self.self_weak.borrow().clone();
            let provider = provider.clone();
            let account = account.clone();
            move |_| {
                if let Some(settings) = weak.upgrade() {
                    settings
                        .active_detail
                        .borrow_mut()
                        .hide(&provider, &account);
                }
            }
        });
        self.active_detail.borrow_mut().show(&provider, &account);
        self.dialog.push_subpage(detail.page());
        self.details
            .borrow_mut()
            .insert(&provider, &account, detail);
    }

    fn confirm_removal(self: &Rc<Self>, provider: String, account: String) {
        let definition = self
            .definitions
            .borrow()
            .iter()
            .find(|definition| definition.provider == provider)
            .cloned();
        let Some(definition) = definition else {
            return;
        };
        let settings = Rc::clone(self);
        glib::spawn_future_local(async move {
            let confirmation = adw::AlertDialog::builder()
                .heading(format!("Remove {}?", definition.title))
                .body(
                    "This removes the provider and its saved credentials. Quota history will be kept.",
                )
                .build();
            confirmation.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
            confirmation.set_default_response(Some("cancel"));
            confirmation.set_close_response("cancel");
            confirmation.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            if confirmation.choose_future(Some(&settings.dialog)).await == "remove" {
                match settings.proxy.remove_provider(&provider, &account).await {
                    Ok(()) => {
                        remove_local_provider(
                            &mut settings.statuses.borrow_mut(),
                            &mut settings.local_added.borrow_mut(),
                            &mut settings.details.borrow_mut(),
                            &provider,
                            &account,
                        );
                        settings.refresh_views();
                    }
                    Err(error) => settings.toast(&reason(&error)),
                }
            }
        });
    }

    fn toast(&self, message: &str) {
        self.dialog.add_toast(adw::Toast::new(message));
    }

    fn cancel_pending_logins(&self) {
        self.pending.close();
        let pending = self.pending.take_all();
        if pending.is_empty() {
            return;
        }
        let proxy = self.proxy.clone();
        glib::spawn_future_local(async move {
            for (provider, account) in pending {
                let _ = proxy.cancel_login(&provider, &account).await;
            }
        });
    }
}

fn pending_status(definition: &ProviderDefinition, account: &str) -> ProviderStatus {
    let mut status = ProviderStatus::pending(
        &ProviderId::new(&definition.provider),
        &AccountId::new(account),
    );
    status.credential = Some(definition.credential.clone());
    status.credential_hint = Some(definition.credential_hint.clone());
    status.options = definition.options.clone();
    status
}

/// Whether a provider has a configuration detail worth navigating to after it is added.
///
/// A browser-session provider with no options starts polling immediately. Pushing an empty
/// detail page in that case looks like the click did nothing; the configured list and card
/// are the useful confirmation instead. Choosing among local authentication sources counts
/// as something to configure even when there is neither a credential nor an ordinary
/// setting: which login on this machine to read is the whole decision.
pub(super) fn opens_detail_after_add(definition: &ProviderDefinition) -> bool {
    definition.browser_auth.is_some()
        || definition.credential != CredentialKind::None.as_wire()
        || !definition.options.is_empty()
}

/// Whether a user can give this provider another account: a key or a browser login is
/// something a second account can hold its own copy of, while an external or
/// credential-free provider reads whatever one thing this machine already has.
pub(super) fn multi_account_capable(definition: &ProviderDefinition) -> bool {
    matches!(
        definition.credential_kind(),
        Some(CredentialKind::Key | CredentialKind::OAuth)
    )
}

/// Whether a typed name can be confirmed: it must suggest an id the config can hold, and
/// a rename must suggest one the account does not already have.
pub(super) fn name_suggests_usable(name: &str, current: Option<&str>) -> bool {
    let slug = account_slug_suggestion(name);
    valid_account_slug(&slug) && current.is_none_or(|held| held != slug)
}

/// The name-to-id entry dialog the provider row's "+" and the account label's pen share.
///
/// The id, not the name, is what gets stored — so the id the name suggests is previewed
/// live under the entry, and confirm stays disabled until the name suggests an id the
/// config can hold (and, for a rename, one the account does not already have). Returns
/// the suggested id, or `None` when the dialog was dismissed.
pub(super) async fn name_dialog(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    prefill: &str,
    confirm: &str,
    current: Option<&str>,
) -> Option<String> {
    let entry = gtk::Entry::builder()
        .placeholder_text("Account name")
        .text(prefill)
        .activates_default(true)
        .build();
    // Owned rather than borrowed: the entry keeps validating after this returns, and the
    // id a rename may not re-suggest has to live as long as the preview does.
    let current = current.map(str::to_owned);
    let preview = gtk::Label::builder()
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();
    let dialog = adw::AlertDialog::builder().heading(heading).build();
    dialog.add_responses(&[("cancel", "Cancel"), ("accept", confirm)]);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);

    let refresh_preview = {
        let entry = entry.clone();
        let preview = preview.clone();
        let dialog = dialog.clone();
        move || {
            let name = entry.text().to_string();
            let slug = account_slug_suggestion(&name);
            preview.set_text(&format!("Account id: {slug}"));
            dialog.set_response_enabled("accept", name_suggests_usable(&name, current.as_deref()));
        }
    };
    entry.connect_changed({
        let refresh_preview = refresh_preview.clone();
        move |_| refresh_preview()
    });
    refresh_preview();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    content.append(&entry);
    content.append(&preview);
    dialog.set_extra_child(Some(&content));

    (dialog.choose_future(Some(parent)).await == "accept")
        .then(|| account_slug_suggestion(&entry.text()))
}

/// A D-Bus error as one sentence for a toast.
pub(super) fn reason(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(_, Some(detail), _) => detail.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tidemark_types::{
        AccountId, AuthMode, AuthSelector, CredentialKind, ProviderDefinition, ProviderId,
        ProviderStatus,
    };

    use super::detail::{AfterBeginAction, after_begin_action};
    use super::{
        ActiveDetail, DEFAULT_ACCOUNT, DetailCache, PendingLogins, multi_account_capable,
        name_suggests_usable, opens_detail_after_add, remove_local_provider,
    };

    fn definition(credential: CredentialKind) -> ProviderDefinition {
        ProviderDefinition {
            provider: "zai".into(),
            title: "Z.ai".into(),
            credential: credential.as_wire().into(),
            credential_hint: String::new(),
            external: None,
            browser_auth: None,
            options: Vec::new(),
        }
    }

    /// A Cursor-shaped catalog entry: no credential to paste or sign in to, but an explicit
    /// local source to pick on its own authentication page.
    fn keyless_with_browser_auth() -> ProviderDefinition {
        let mut definition = definition(CredentialKind::None);
        definition.provider = "cursor".into();
        definition.browser_auth = Some(AuthSelector {
            option: "auth-source".into(),
            modes: vec![
                AuthMode {
                    value: "cursor-app".into(),
                    title: "Cursor App".into(),
                },
                AuthMode {
                    value: "browser".into(),
                    title: "Browser".into(),
                },
            ],
        });
        definition
    }

    #[test]
    fn a_keyless_provider_without_sources_or_options_returns_to_the_list_after_adding() {
        assert!(!opens_detail_after_add(&definition(CredentialKind::None)));
    }

    #[test]
    fn a_keyless_provider_that_picks_a_local_source_opens_its_detail_after_adding() {
        // Nothing was typed or signed in to, yet there is something to configure: choosing
        // which login on this machine to read is the whole point of adding it.
        assert!(opens_detail_after_add(&keyless_with_browser_auth()));
    }

    #[test]
    fn a_provider_with_a_credential_or_options_opens_its_detail_after_adding() {
        assert!(opens_detail_after_add(&definition(CredentialKind::Key)));
        let mut with_options = definition(CredentialKind::None);
        with_options.options.push(tidemark_types::ProviderOption {
            name: "model".into(),
            title: "Model".into(),
            description: None,
            value: String::new(),
            choices: Vec::new(),
        });
        assert!(opens_detail_after_add(&with_options));
    }

    #[test]
    fn key_and_oauth_providers_can_hold_more_than_one_account() {
        // A key or a browser login is something a second account can hold its own copy
        // of; those are the providers the "+" is drawn on.
        assert!(multi_account_capable(&definition(CredentialKind::Key)));
        assert!(multi_account_capable(&definition(CredentialKind::OAuth)));
    }

    #[test]
    fn external_and_credential_free_providers_cannot_hold_a_second_account() {
        assert!(!multi_account_capable(&definition(
            CredentialKind::External
        )));
        assert!(!multi_account_capable(&definition(CredentialKind::None)));
        let mut unknown = definition(CredentialKind::None);
        unknown.credential = "future-kind".into();
        assert!(!multi_account_capable(&unknown));
    }

    #[test]
    fn a_name_can_only_be_confirmed_when_it_suggests_a_fresh_id() {
        assert!(!name_suggests_usable("", None));
        assert!(!name_suggests_usable("   ", None));
        assert!(name_suggests_usable("My Work", None));
        // A rename to the id the account already has is the daemon's no-op refusal, and
        // the confirm button is where the user should hear that from.
        assert!(!name_suggests_usable("My Work", Some("my-work")));
        assert!(name_suggests_usable("My Team", Some("my-work")));
    }

    #[test]
    fn pending_logins_are_taken_once_for_cancellation() {
        let pending = PendingLogins::default();
        pending.insert("antigravity", "default");
        assert_eq!(
            pending.take_all(),
            vec![("antigravity".into(), "default".into())]
        );
        assert!(pending.take_all().is_empty());
    }

    #[test]
    fn closing_the_tracker_rejects_a_login_that_has_not_started_yet() {
        let pending = PendingLogins::default();
        pending.close();

        assert!(!pending.insert("antigravity", "default"));
        assert!(pending.take_all().is_empty());
    }

    #[test]
    fn repeated_waiting_update_after_begin_reaches_browser_and_await() {
        let pending = PendingLogins::default();
        assert!(pending.insert("claude", "default"));

        let accepted = pending.insert("claude", "default");

        assert_eq!(
            after_begin_action(accepted),
            AfterBeginAction::OpenBrowserAndAwait
        );
    }

    #[test]
    fn begin_login_completion_after_dialog_close_requests_cancellation() {
        let pending = PendingLogins::default();
        pending.close();

        let accepted_after_begin = pending.insert("claude", "default");

        assert_eq!(
            after_begin_action(accepted_after_begin),
            AfterBeginAction::CancelLogin
        );
        assert!(pending.take_all().is_empty());
    }

    #[test]
    fn remove_then_readd_uses_a_fresh_detail_value() {
        let mut details = DetailCache::default();
        let mut statuses = Vec::new();
        let mut local_added = HashSet::from([("zai".to_owned(), DEFAULT_ACCOUNT.to_owned())]);
        details.insert("zai", "default", "old credential page");

        remove_local_provider(
            &mut statuses,
            &mut local_added,
            &mut details,
            "zai",
            "default",
        );
        details.insert("zai", "default", "fresh pending page");

        assert_eq!(details.get("zai", "default"), Some(&"fresh pending page"));
        assert_eq!(details.values().count(), 1);
        assert!(!local_added.contains(&("zai".to_owned(), DEFAULT_ACCOUNT.to_owned())));
    }

    #[test]
    fn an_externally_removed_active_detail_requests_a_return_to_the_configured_list() {
        let mut active = ActiveDetail::default();
        active.show("zai", "default");

        assert!(active.take_if_missing(&[]));
        assert_eq!(active.identity(), None);
    }

    #[test]
    fn a_renamed_active_detail_is_retired_when_only_its_successor_remains() {
        // A rename announces the old id's removal and the new id's arrival; the page keyed
        // by the old id must return to the list rather than pretend to be its successor.
        let mut active = ActiveDetail::default();
        active.show("kimi", "work");
        let successor = vec![ProviderStatus::pending(
            &ProviderId::new("kimi"),
            &AccountId::new("team"),
        )];

        assert!(active.take_if_missing(&successor));
        assert_eq!(active.identity(), None);
    }
}
