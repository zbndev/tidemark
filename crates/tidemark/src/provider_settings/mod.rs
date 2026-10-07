//! Navigable provider configuration: the configured list on two tabs, the catalog picker,
//! and one account's page, drawn over the main window as libadwaita's preferences dialog
//! is.
//!
//! The built-in tab's "+" opens the catalog picker and the custom tab's imports a plugin
//! file. A plugin lives on the custom tab in both of its states: configured, where it is
//! drawn exactly like a built-in provider, and installed-but-unconfigured, where it is a
//! row waiting for its first account.

mod browser_auth;
mod detail;
mod list;
pub mod model;
pub mod plugins;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tidemark_types::{
    AccountId, CredentialKind, PluginInfo, ProviderDefinition, ProviderId, ProviderStatus,
    account_id_suggestion, valid_account_id,
};

use self::browser_auth::{BrowserAuth, Tone};
use self::detail::{Auth, Local, SOURCE, override_key};
use crate::alert::{Alerts, Appearance, Content, Question};
use crate::bus::DaemonProxy;
use crate::marks::Marks;
use crate::window::spawn;
use crate::{
    AppWindow, CandidateData, DetailData, MenuEntry, OptionData, PickerRowData, ProviderRowData,
    ProviderSettings, SwitchData,
};

/// The account a provider's structural first add holds. The daemon will not rename it,
/// so the label pen stays off its page.
pub(crate) const DEFAULT_ACCOUNT: &str = "default";

type Identity = (String, String);

fn identity(provider: &str, account: &str) -> Identity {
    (provider.to_owned(), account.to_owned())
}

/// OAuth attempts which the open dialog is responsible for cancelling on close.
#[derive(Debug, Default)]
struct PendingLogins {
    identities: RefCell<HashSet<Identity>>,
    closed: Cell<bool>,
}

impl PendingLogins {
    /// False once the dialog has closed: a login that finishes starting after that has
    /// nobody to wait for it, and is cancelled instead.
    fn insert(&self, provider: &str, account: &str) -> bool {
        if self.closed.get() {
            return false;
        }
        self.identities
            .borrow_mut()
            .insert(identity(provider, account));
        true
    }

    fn remove(&self, provider: &str, account: &str) {
        self.identities
            .borrow_mut()
            .remove(&identity(provider, account));
    }

    fn contains(&self, provider: &str, account: &str) -> bool {
        self.identities
            .borrow()
            .contains(&identity(provider, account))
    }

    fn take_all(&self) -> Vec<Identity> {
        self.identities.borrow_mut().drain().collect()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Page {
    List,
    Picker,
    Detail(Identity),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    BuiltIn,
    Custom,
}

/// What confirming a plugin's account form would create.
///
/// Carried into the form rather than acted on before it, so a dismissed form leaves the
/// configuration exactly as it found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginTarget {
    /// Nothing is configured for this plugin yet: confirming adds the provider, which
    /// comes with its `default` account.
    NewProvider,
    /// The provider is configured; confirming names and adds another account.
    NewAccount,
}

/// The models an account's page draws its lists from, kept for the dialog's life so a
/// status update changes rows in place instead of rebuilding the page under the pointer.
#[derive(Default)]
struct DetailModels {
    choices: Rc<VecModel<SharedString>>,
    modes: Rc<VecModel<SharedString>>,
    candidates: Rc<VecModel<CandidateData>>,
    options: Rc<VecModel<OptionData>>,
    notifications: Rc<VecModel<SwitchData>>,
}

/// Owns everything one open provider dialog shows. A closed one draws nothing, whatever
/// its unfinished tasks try.
pub struct ProviderDialog {
    ui: slint::Weak<AppWindow>,
    daemon: Rc<dyn Fn() -> Option<DaemonProxy<'static>>>,
    alerts: Rc<Alerts>,
    marks: Rc<Marks>,
    definitions: RefCell<Vec<ProviderDefinition>>,
    statuses: RefCell<Vec<ProviderStatus>>,
    /// Accounts added here whose first status the daemon has not published yet.
    local_added: RefCell<HashSet<Identity>>,
    tab: Cell<Tab>,
    page: RefCell<Page>,
    search: RefCell<String>,
    collapsed: RefCell<HashSet<String>>,
    pending: PendingLogins,
    login_urls: RefCell<HashMap<Identity, String>>,
    /// Controls a click has moved before the daemon has answered, per account.
    overrides: RefCell<HashMap<Identity, HashMap<String, String>>>,
    browser: RefCell<HashMap<Identity, BrowserAuth>>,
    rows: Rc<VecModel<ProviderRowData>>,
    picker: Rc<VecModel<PickerRowData>>,
    detail: DetailModels,
    on_closed: Box<dyn Fn()>,
    closed: Cell<bool>,
}

impl std::fmt::Debug for ProviderDialog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderDialog")
            .field("page", &self.page.borrow())
            .field("tab", &self.tab.get())
            .field("closed", &self.closed.get())
            .finish_non_exhaustive()
    }
}

impl ProviderDialog {
    pub fn open(
        ui: &AppWindow,
        daemon: Rc<dyn Fn() -> Option<DaemonProxy<'static>>>,
        alerts: Rc<Alerts>,
        marks: Rc<Marks>,
        definitions: &[ProviderDefinition],
        statuses: &[ProviderStatus],
        on_closed: impl Fn() + 'static,
    ) -> Rc<Self> {
        let dialog = Rc::new(Self {
            ui: ui.as_weak(),
            daemon,
            alerts,
            marks,
            definitions: RefCell::default(),
            statuses: RefCell::default(),
            local_added: RefCell::default(),
            // The catalog is the common case, and a custom provider is a destination
            // rather than a default.
            tab: Cell::new(Tab::BuiltIn),
            page: RefCell::new(Page::List),
            search: RefCell::default(),
            collapsed: RefCell::default(),
            pending: PendingLogins::default(),
            login_urls: RefCell::default(),
            overrides: RefCell::default(),
            browser: RefCell::default(),
            rows: Rc::default(),
            picker: Rc::default(),
            detail: DetailModels::default(),
            on_closed: Box::new(on_closed),
            closed: Cell::new(false),
        });
        dialog.wire(ui);
        let settings = ui.global::<ProviderSettings>();
        settings.set_rows(ModelRc::from(Rc::clone(&dialog.rows)));
        settings.set_picker(ModelRc::from(Rc::clone(&dialog.picker)));
        dialog.apply(definitions, statuses);
        settings.set_open(true);
        dialog
    }

    fn wire(self: &Rc<Self>, ui: &AppWindow) {
        let settings = ui.global::<ProviderSettings>();
        let weak = Rc::downgrade(self);
        let act = |action: fn(&Rc<Self>)| {
            let weak = weak.clone();
            move || {
                if let Some(dialog) = weak.upgrade() {
                    action(&dialog);
                }
            }
        };
        let with = |action: fn(&Rc<Self>, String)| {
            let weak = weak.clone();
            move |argument: SharedString| {
                if let Some(dialog) = weak.upgrade() {
                    action(&dialog, argument.into());
                }
            }
        };
        let indexed = |action: fn(&Rc<Self>, usize)| {
            let weak = weak.clone();
            move |index: i32| {
                if let (Some(dialog), Ok(index)) = (weak.upgrade(), usize::try_from(index)) {
                    action(&dialog, index);
                }
            }
        };

        settings.on_filled(|text| !text.trim().is_empty());
        settings.on_close(act(Self::close));
        settings.on_back(act(|dialog| dialog.show(Page::List)));
        settings.on_choose_tab(indexed(|dialog, index| {
            dialog.tab.set(if index == 0 {
                Tab::BuiltIn
            } else {
                Tab::Custom
            });
            dialog.render();
        }));
        settings.on_add(act(|dialog| match dialog.tab.get() {
            Tab::BuiltIn => {
                dialog.search.borrow_mut().clear();
                dialog.show(Page::Picker);
            }
            Tab::Custom => dialog.import_plugin(),
        }));
        settings.on_search(with(|dialog, query| {
            dialog.search.replace(query);
            dialog.render();
        }));
        settings.on_pick(with(Self::add_provider));
        settings.on_row_menu({
            let weak = weak.clone();
            move |provider, unconfigured| {
                let Some(dialog) = weak.upgrade() else {
                    return ModelRc::default();
                };
                let actions = if unconfigured {
                    vec![CardAction::AddAccount, CardAction::Remove]
                } else {
                    card_actions(dialog.definition(&provider).as_ref())
                };
                ModelRc::from(menu_entries(actions).as_slice())
            }
        });
        settings.on_row_action({
            let weak = weak.clone();
            move |id, provider, account, unconfigured| {
                let (Some(dialog), Some(action)) = (weak.upgrade(), CardAction::from_id(&id))
                else {
                    return;
                };
                if unconfigured {
                    match action {
                        CardAction::AddAccount => dialog.add_provider(provider.into()),
                        CardAction::Remove => dialog.confirm_plugin_removal(provider.into()),
                        CardAction::Modify => {}
                    }
                } else {
                    dialog.shortcut(action, &provider, &account);
                }
            }
        });
        settings.on_toggle_expanded(with(|dialog, provider| {
            {
                let mut collapsed = dialog.collapsed.borrow_mut();
                if !collapsed.remove(&provider) {
                    collapsed.insert(provider);
                }
            }
            dialog.render();
        }));

        settings.on_rename(act(Self::rename));
        settings.on_save_key(with(Self::save_key));
        settings.on_remove_key(act(|dialog| dialog.sign_out("Key removed.")));
        settings.on_sign_in(act(Self::sign_in));
        settings.on_sign_out(act(|dialog| {
            dialog.sign_out("Signed out of Tidemark's account.");
        }));
        settings.on_cancel_login(act(Self::cancel_login));
        settings.on_choose_source(indexed(Self::choose_source));
        settings.on_recheck(act(Self::recheck));
        settings.on_choose_mode(indexed(|dialog, index| {
            if let Some((identity, _, _)) = dialog.current()
                && let Some(browser) = dialog.browser.borrow_mut().get_mut(&identity)
            {
                browser.choose_mode(index);
            }
            dialog.render();
        }));
        settings.on_choose_candidate(with(Self::choose_candidate));
        settings.on_paste_session(with(Self::paste_session));
        settings.on_inspect(act(Self::inspect));
        settings.on_choose_option({
            let weak = weak.clone();
            move |name, index| {
                if let (Some(dialog), Ok(index)) = (weak.upgrade(), usize::try_from(index)) {
                    dialog.choose_option(name.into(), index);
                }
            }
        });
        settings.on_toggle_notify({
            let weak = weak.clone();
            move |key, enabled| {
                if let Some(dialog) = weak.upgrade() {
                    dialog.toggle_notify(key.into(), enabled);
                }
            }
        });
    }

    /// Takes the daemon's catalog and statuses. Accounts added here stay drawn until their
    /// first status arrives; a control moved ahead of its write keeps its position until
    /// the account's status changes, which is the daemon's answer to it.
    pub fn apply(&self, definitions: &[ProviderDefinition], statuses: &[ProviderStatus]) {
        if self.closed.get() {
            return;
        }
        self.definitions.replace(definitions.to_vec());
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
        {
            let previous = self.statuses.borrow();
            let find = |statuses: &[ProviderStatus], (provider, account): &Identity| {
                statuses
                    .iter()
                    .find(|status| &status.provider == provider && &status.account == account)
                    .cloned()
            };
            self.overrides
                .borrow_mut()
                .retain(|identity, _| find(&previous, identity) == find(&merged, identity));
            let mut browser = self.browser.borrow_mut();
            browser.retain(|identity, _| find(&merged, identity).is_some());
            for (identity, auth) in browser.iter_mut() {
                if let Some(status) = find(&merged, identity) {
                    auth.apply_selection(status.auth_selection.as_ref());
                }
            }
        }
        // A page whose account is gone — removed elsewhere, or renamed, which retires the
        // old id — returns to the list rather than pretending to be its successor.
        let gone = matches!(
            &*self.page.borrow(),
            Page::Detail((provider, account)) if !merged
                .iter()
                .any(|status| &status.provider == provider && &status.account == account)
        );
        self.statuses.replace(merged);
        if gone {
            self.page.replace(Page::List);
        }
        self.render();
    }

    fn show(&self, page: Page) {
        self.page.replace(page);
        self.render();
    }

    fn definition(&self, provider: &str) -> Option<ProviderDefinition> {
        self.definitions
            .borrow()
            .iter()
            .find(|definition| definition.provider == provider)
            .cloned()
    }

    fn status(&self, provider: &str, account: &str) -> Option<ProviderStatus> {
        self.statuses
            .borrow()
            .iter()
            .find(|status| status.provider == provider && status.account == account)
            .cloned()
    }

    /// The account whose page is showing, with its definition and status.
    fn current(&self) -> Option<(Identity, ProviderDefinition, ProviderStatus)> {
        let Page::Detail(identity) = self.page.borrow().clone() else {
            return None;
        };
        let definition = self.definition(&identity.0)?;
        let status = self.status(&identity.0, &identity.1)?;
        Some((identity, definition, status))
    }

    fn proxy(&self) -> Option<DaemonProxy<'static>> {
        let proxy = (self.daemon)();
        if proxy.is_none() {
            self.toast("Tidemark is not running.");
        }
        proxy
    }

    fn toast(&self, message: &str) {
        if self.closed.get() {
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<ProviderSettings>()
                .invoke_show_toast(message.into());
        }
    }

    fn render(&self) {
        if self.closed.get() {
            return;
        }
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let settings = ui.global::<ProviderSettings>();
        settings.set_tab(match self.tab.get() {
            Tab::BuiltIn => 0,
            Tab::Custom => 1,
        });
        self.render_list(&settings);
        let page = self.page.borrow().clone();
        match page {
            Page::List => settings.set_page(0),
            Page::Picker => {
                self.render_picker();
                settings.set_page(1);
            }
            Page::Detail(_) => {
                self.render_detail(&settings);
                settings.set_page(2);
            }
        }
    }

    fn render_list(&self, settings: &ProviderSettings<'_>) {
        let definitions = self.definitions.borrow();
        let statuses = self.statuses.borrow();
        // A plugin's provider id is the one spelling of its tab membership: statuses carry
        // no plugin flag of their own, so the definition decides which tab it is drawn on.
        let plugins: HashSet<&str> = definitions
            .iter()
            .filter(|definition| definition.plugin.is_some())
            .map(|definition| definition.provider.as_str())
            .collect();
        let custom = self.tab.get() == Tab::Custom;
        let shown: Vec<ProviderStatus> = statuses
            .iter()
            .filter(|status| plugins.contains(status.provider.as_str()) == custom)
            .cloned()
            .collect();
        // Installed definitions no account uses are the custom tab's other state: a
        // provider waiting for its first account rather than a hidden file.
        let unconfigured: Vec<ProviderDefinition> = if custom {
            definitions
                .iter()
                .filter(|definition| {
                    definition.plugin.is_some()
                        && !statuses
                            .iter()
                            .any(|status| status.provider == definition.provider)
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let rows = list::rows(
            &definitions,
            &shown,
            &unconfigured,
            &self.collapsed.borrow(),
            &|provider, account| self.pending.contains(provider, account),
        );
        let rows: Vec<ProviderRowData> = rows
            .into_iter()
            .map(|row| {
                let mark = self.marks.get(&row.provider);
                ProviderRowData {
                    kind: match row.kind {
                        list::Kind::Provider => 0,
                        list::Kind::Account => 1,
                        list::Kind::Unconfigured => 2,
                    },
                    provider: row.provider.into(),
                    account: row.account.into(),
                    title: row.title.into(),
                    subtitle: row.subtitle.into(),
                    has_mark: mark.is_some(),
                    mark: mark.unwrap_or_default(),
                    nested: row.nested,
                    expanded: row.expanded,
                }
            })
            .collect();
        sync(&self.rows, rows, |a, b| {
            let bare = |row: &ProviderRowData| ProviderRowData {
                mark: blank(),
                ..row.clone()
            };
            same_image(&a.mark, &b.mark) && bare(a) == bare(b)
        });
        let (title, description) = if custom {
            ("No custom providers", "Use + to import a provider file.")
        } else {
            ("No providers added", "Use + to add a provider.")
        };
        settings.set_empty_title(title.into());
        settings.set_empty_description(description.into());
    }

    fn render_picker(&self) {
        let rows: Vec<PickerRowData> = model::addable(
            &self.definitions.borrow(),
            &self.statuses.borrow(),
            &self.search.borrow(),
        )
        .into_iter()
        .map(|definition| {
            let mark = self.marks.get(&definition.provider);
            PickerRowData {
                provider: definition.provider.clone().into(),
                title: definition.title.clone().into(),
                has_mark: mark.is_some(),
                mark: mark.unwrap_or_default(),
            }
        })
        .collect();
        sync(&self.picker, rows, |a, b| {
            a.provider == b.provider && a.title == b.title && same_image(&a.mark, &b.mark)
        });
    }

    fn render_detail(&self, settings: &ProviderSettings<'_>) {
        let Some((identity, definition, status)) = self.current() else {
            return;
        };
        let urls = self.login_urls.borrow();
        let overrides = self.overrides.borrow();
        let browser = self.browser.borrow();
        let view = detail::view(
            &definition,
            &status,
            &Local {
                login_url: urls.get(&identity).map(String::as_str),
                overrides: overrides.get(&identity),
                browser: browser.get(&identity),
            },
        );

        let strings = |values: &[String]| {
            values
                .iter()
                .map(SharedString::from)
                .collect::<Vec<SharedString>>()
        };
        sync(&self.detail.choices, strings(&view.choices), PartialEq::eq);
        sync(&self.detail.modes, strings(&view.modes), PartialEq::eq);
        let candidates: Vec<CandidateData> = view
            .lines
            .iter()
            .map(|line| CandidateData {
                id: line.id.clone().into(),
                title: line.title.clone().into(),
                subtitle: line.subtitle.clone().into(),
                word: browser_auth::state_word(line.state).into(),
                tone: match browser_auth::state_tone(line.state) {
                    Tone::Plain => 0,
                    Tone::Working => 1,
                    Tone::Broken => 2,
                },
                selectable: line.selectable,
                in_use: line.in_use,
                indent: line.indent,
                note: line.note,
            })
            .collect();
        sync(&self.detail.candidates, candidates, PartialEq::eq);
        let options: Vec<OptionData> = view
            .options
            .iter()
            .map(|option| OptionData {
                name: option.name.clone().into(),
                title: option.title.clone().into(),
                description: option.description.clone().into(),
                choices: ModelRc::new(VecModel::from(strings(&option.choices))),
                selected: option.selected as i32,
            })
            .collect();
        sync(&self.detail.options, options, |a, b| {
            a.name == b.name
                && a.title == b.title
                && a.description == b.description
                && a.selected == b.selected
                && a.choices.iter().eq(b.choices.iter())
        });
        let notifications: Vec<SwitchData> = view
            .notifications
            .iter()
            .map(|row| SwitchData {
                key: row.key.clone().into(),
                title: row.title.clone().into(),
                enabled: row.enabled,
            })
            .collect();
        sync(&self.detail.notifications, notifications, PartialEq::eq);

        let mark = self.marks.get(&definition.provider);
        let local = view.local.clone().unwrap_or_else(|| detail::LocalLogin {
            label: String::new(),
            location: String::new(),
            present: None,
            presence_text: String::new(),
            command: String::new(),
            note: String::new(),
        });
        let data = DetailData {
            provider: identity.0.clone().into(),
            account: identity.1.clone().into(),
            title: view.title.into(),
            subtitle: view.subtitle.into(),
            has_mark: mark.is_some(),
            mark: mark.unwrap_or_default(),
            can_rename: view.can_rename,
            auth_shown: view.auth.is_some(),
            description: view.description.into(),
            auth: match view.auth {
                Some(Auth::Key) | None => 0,
                Some(Auth::Login) => 1,
                Some(Auth::Described) => 2,
                Some(Auth::LocalSource) => 3,
            },
            stored: view.stored,
            choices: ModelRc::from(Rc::clone(&self.detail.choices)),
            choice: view.choice as i32,
            tidemark_half: view.tidemark_half,
            sign_in_subtitle: view.sign_in_subtitle.into(),
            signed_in: view.stored,
            waiting: view.waiting,
            login_url: view.login_url.into(),
            local_label: local.label.into(),
            local_location: local.location.into(),
            presence: match local.present {
                None => 0,
                Some(true) => 1,
                Some(false) => 2,
            },
            presence_text: local.presence_text.into(),
            command: local.command.into(),
            note: local.note.into(),
            connection: view.connection.into(),
            modes: ModelRc::from(Rc::clone(&self.detail.modes)),
            mode: view.mode as i32,
            paste: view.pasting,
            candidates: ModelRc::from(Rc::clone(&self.detail.candidates)),
            options: ModelRc::from(Rc::clone(&self.detail.options)),
            notifications: ModelRc::from(Rc::clone(&self.detail.notifications)),
        };
        let shown = settings.get_detail();
        let bare = |data: &DetailData| DetailData {
            mark: blank(),
            ..data.clone()
        };
        if !(same_image(&shown.mark, &data.mark) && bare(&shown) == bare(&data)) {
            settings.set_detail(data);
        }
    }

    fn close(self: &Rc<Self>) {
        if self.closed.replace(true) {
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<ProviderSettings>().set_open(false);
        }
        self.pending.close();
        let pending = self.pending.take_all();
        if !pending.is_empty()
            && let Some(proxy) = (self.daemon)()
        {
            spawn(async move {
                for (provider, account) in pending {
                    let _ = proxy.cancel_login(&provider, &account).await;
                }
            });
        }
        (self.on_closed)();
    }

    fn add_provider(self: &Rc<Self>, provider: String) {
        let dialog = Rc::clone(self);
        spawn(async move {
            let definition = dialog.definition(&provider);
            // A plugin has nowhere to send anything until an endpoint is filed, and its
            // page has no field for one. So the form comes first and adds the provider
            // itself: a user who changes their mind is left with nothing to clean up.
            if let Some(definition) = definition.as_ref()
                && let Some(info) = definition.plugin.clone()
            {
                if let Some(account) = dialog
                    .configure_plugin_account(
                        &definition.provider,
                        &definition.title,
                        &info,
                        PluginTarget::NewProvider,
                    )
                    .await
                {
                    dialog.note_local_account(&provider, &account);
                }
                return;
            }
            let Some(proxy) = dialog.proxy() else {
                return;
            };
            if let Err(error) = proxy.add_provider(&provider).await {
                dialog.toast(&reason(&error));
                return;
            }
            let Some(definition) = definition else {
                return;
            };
            dialog.note_local_account(&provider, DEFAULT_ACCOUNT);
            dialog.show(Page::List);
            if opens_detail_after_add(&definition) {
                dialog.open_detail(provider, AccountId::default().to_string());
            }
        });
    }

    /// The provider row's "+": asks for a name, has the daemon add the account it
    /// suggests, and lands on that account's page to give it a credential.
    fn add_account(self: &Rc<Self>, provider: String) {
        let Some(definition) = self.definition(&provider) else {
            return;
        };
        let dialog = Rc::clone(self);
        spawn(async move {
            let slug = match definition.plugin.clone() {
                // One form: the account's name, where its key is sent, and the key. The
                // three are one decision, and the form has added the account by the time
                // it answers.
                Some(info) => {
                    let Some(slug) = dialog
                        .configure_plugin_account(
                            &definition.provider,
                            &definition.title,
                            &info,
                            PluginTarget::NewAccount,
                        )
                        .await
                    else {
                        return;
                    };
                    slug
                }
                None => {
                    let answer = dialog
                        .alerts
                        .ask(Question::suggested(
                            format!("New {} account", definition.title),
                            "Add",
                            Content::Name {
                                prefill: String::new(),
                                current: None,
                            },
                        ))
                        .await;
                    if !answer.confirmed() {
                        return;
                    }
                    let slug = answer.account_id();
                    let Some(proxy) = dialog.proxy() else {
                        return;
                    };
                    if let Err(error) = proxy.add_account(&provider, &slug).await {
                        dialog.toast(&reason(&error));
                        return;
                    }
                    slug
                }
            };
            dialog.note_local_account(&provider, &slug);
            dialog.open_detail(provider, slug);
        });
    }

    fn open_detail(self: &Rc<Self>, provider: String, account: String) {
        let (Some(definition), Some(status)) =
            (self.definition(&provider), self.status(&provider, &account))
        else {
            return;
        };
        let identity = identity(&provider, &account);
        let inspects = if let Some(selector) = &definition.browser_auth {
            self.browser
                .borrow_mut()
                .entry(identity.clone())
                .or_insert_with(|| {
                    BrowserAuth::new(&selector.modes, status.auth_selection.as_ref())
                });
            true
        } else {
            false
        };
        self.show(Page::Detail(identity));
        // Opening counts as asking: the report is what turns Checking… into rows.
        if inspects {
            self.inspect();
        }
    }

    /// A card or configured row's menu action, taken with the account's identity so
    /// adding, editing and removing have one implementation. The tab is settled first,
    /// so going back from the account's page lands on the tab its provider is on.
    pub fn shortcut(self: &Rc<Self>, action: CardAction, provider: &str, account: &str) {
        let custom = self
            .definition(provider)
            .is_some_and(|definition| definition.plugin.is_some());
        self.tab
            .set(if custom { Tab::Custom } else { Tab::BuiltIn });
        self.show(Page::List);
        match action {
            CardAction::AddAccount => self.add_account(provider.to_owned()),
            CardAction::Modify => self.open_detail(provider.to_owned(), account.to_owned()),
            CardAction::Remove => self.confirm_removal(provider.to_owned(), account.to_owned()),
        }
    }

    fn confirm_removal(self: &Rc<Self>, provider: String, account: String) {
        let Some(definition) = self.definition(&provider) else {
            return;
        };
        // A plugin's card is the provider: when the trash takes its last account, the
        // installed file goes with it, so the confirmation says so and the removal does it.
        let removes_plugin = definition.plugin.is_some()
            && self
                .statuses
                .borrow()
                .iter()
                .filter(|status| status.provider == provider)
                .count()
                == 1;
        let dialog = Rc::clone(self);
        spawn(async move {
            let answer = dialog
                .alerts
                .ask(Question::destructive(
                    format!("Remove {}?", definition.title),
                    if removes_plugin {
                        "This removes the provider, its saved credentials and the installed \
                         provider file. Quota history will be kept."
                    } else {
                        "This removes the provider and its saved credentials. Quota history \
                         will be kept."
                    },
                    "Remove",
                ))
                .await;
            if !answer.confirmed() {
                return;
            }
            let Some(proxy) = dialog.proxy() else {
                return;
            };
            if let Err(error) = proxy.remove_provider(&provider, &account).await {
                dialog.toast(&reason(&error));
                return;
            }
            let identity = identity(&provider, &account);
            dialog.local_added.borrow_mut().remove(&identity);
            dialog
                .statuses
                .borrow_mut()
                .retain(|status| status.provider != provider || status.account != account);
            dialog.overrides.borrow_mut().remove(&identity);
            dialog.browser.borrow_mut().remove(&identity);
            if removes_plugin && let Err(error) = proxy.remove_plugin(&provider).await {
                // The account is already gone; what is left on screen is the
                // unconfigured row, whose own trash finishes the job.
                dialog.toast(&reason(&error));
            }
            if *dialog.page.borrow() == Page::Detail(identity) {
                dialog.page.replace(Page::List);
            }
            dialog.render();
        });
    }

    /// The custom tab's "+": choose a file, show what it declares, install it if asked,
    /// and go straight on to the endpoint and the key.
    ///
    /// The catalog is not patched here. Installing makes the daemon emit `PluginsChanged`,
    /// and the definitions arrive through the same `apply` every other catalog change
    /// does — a locally invented entry would be a second answer waiting to disagree.
    fn import_plugin(self: &Rc<Self>) {
        let dialog = Rc::clone(self);
        spawn(async move {
            let Some(path) = plugins::choose_file().await else {
                return;
            };
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    dialog.toast(&format!("Cannot read {}: {error}", path.display()));
                    return;
                }
            };
            let Some(proxy) = dialog.proxy() else {
                return;
            };
            let info = match proxy.inspect_plugin(bytes.clone()).await {
                Ok(info) => info,
                Err(error) => {
                    dialog
                        .alerts
                        .ask(Question::notice("This file was refused", reason(&error)))
                        .await;
                    return;
                }
            };
            // Two steps on purpose: inspecting writes nothing, so a file somebody sent can
            // be read here and then closed.
            let mark = if info.has_mark {
                match info.mark_svg.as_deref() {
                    Some(svg) => slint::Image::load_from_svg_data(svg.as_bytes()).ok(),
                    // An older daemon only named the materialized mark.
                    None => dialog.marks.get(&info.id),
                }
            } else {
                None
            };
            let answer = dialog
                .alerts
                .ask(Question {
                    heading: format!("Install {}?", info.name),
                    body: "This provider is not part of Tidemark. It runs a parser the file's \
                           author wrote, and it will be sent the API key you give it."
                        .into(),
                    responses: vec![
                        ("cancel", "Cancel", Appearance::Plain),
                        ("confirm", "Install", Appearance::Suggested),
                    ],
                    default: "cancel",
                    close: "cancel",
                    content: Content::Preview {
                        rows: plugins::preview(&info),
                        mark,
                    },
                })
                .await;
            if !answer.confirmed() {
                return;
            }
            let installed = match proxy.install_plugin(bytes).await {
                Ok(installed) => installed,
                Err(error) => {
                    dialog
                        .alerts
                        .ask(Question::notice("This file was refused", reason(&error)))
                        .await;
                    return;
                }
            };
            // An installed definition nobody has pointed at a URL polls nothing and draws
            // no card, so stopping here would leave the user with a success message and no
            // way to guess what is next.
            let id = installed.id.clone();
            let name = installed.name.clone();
            if let Some(account) = dialog
                .configure_plugin_account(&id, &name, &installed, PluginTarget::NewProvider)
                .await
            {
                dialog.note_local_account(&id, &account);
            } else {
                dialog.toast(&format!(
                    "{name} installed. Use + to add an account for it."
                ));
            }
        });
    }

    /// Removes an installed definition that no account uses — the unconfigured row's
    /// trash. A configured plugin is removed through its row instead, which takes the
    /// file with it.
    fn confirm_plugin_removal(self: &Rc<Self>, provider: String) {
        let title = self
            .definition(&provider)
            .map_or_else(|| provider.clone(), |definition| definition.title);
        let dialog = Rc::clone(self);
        spawn(async move {
            let answer = dialog
                .alerts
                .ask(Question::destructive(
                    format!("Remove {title}?"),
                    "This deletes the installed provider file.",
                    "Remove",
                ))
                .await;
            if !answer.confirmed() {
                return;
            }
            let Some(proxy) = dialog.proxy() else {
                return;
            };
            match proxy.remove_plugin(&provider).await {
                Ok(()) => dialog.toast(&format!("{title} removed.")),
                Err(error) => dialog.toast(&reason(&error)),
            }
        });
    }

    /// Fills in one plugin account: the endpoint and the key together, in that order.
    ///
    /// Nothing is written until the form is confirmed, which is why the target says what
    /// *would* be created rather than the caller creating it first. After that the order
    /// is fixed — the account, then the endpoint, then the key, so a key is never stored
    /// for an account with nowhere to send it. Returns the account id on success.
    async fn configure_plugin_account(
        self: &Rc<Self>,
        provider: &str,
        title: &str,
        info: &PluginInfo,
        target: PluginTarget,
    ) -> Option<String> {
        let naming = target == PluginTarget::NewAccount;
        // A second account inherits the endpoint a sibling already sends its key to —
        // the plain-http acknowledgement comes with the URL, because it was the same
        // question about the same destination, already answered.
        let inherited = if naming {
            self.statuses
                .borrow()
                .iter()
                .find(|status| status.provider == provider)
                .and_then(|status| status.plugin_endpoint.clone())
        } else {
            None
        };
        let answer = self
            .alerts
            .ask(Question::suggested(
                match target {
                    PluginTarget::NewProvider => format!("Add {title}"),
                    PluginTarget::NewAccount => format!("New {title} account"),
                },
                "Add",
                Content::PluginAccount {
                    ask_name: naming,
                    ask_endpoint: inherited.is_none(),
                    request: plugins::request_line(info),
                },
            ))
            .await;
        if !answer.confirmed() {
            return None;
        }
        let proxy = self.proxy()?;

        let slug = match target {
            PluginTarget::NewProvider => {
                if let Err(error) = proxy.add_provider(provider).await {
                    self.toast(&reason(&error));
                    return None;
                }
                DEFAULT_ACCOUNT.to_owned()
            }
            PluginTarget::NewAccount => {
                let named = answer.account_id();
                if let Err(error) = proxy.add_account(provider, &named).await {
                    self.toast(&reason(&error));
                    return None;
                }
                named
            }
        };

        let (endpoint, allow_insecure_http) = match &inherited {
            Some(endpoint) => (endpoint.url.clone(), endpoint.allow_insecure_http),
            None => (answer.form.endpoint.trim().to_owned(), answer.form.insecure),
        };
        if let Err(error) = proxy
            .set_plugin_endpoint(provider, &slug, &endpoint, allow_insecure_http)
            .await
        {
            self.toast(&reason(&error));
            return Some(slug);
        }
        if let Err(error) = proxy.set_key(provider, &slug, answer.form.key.trim()).await {
            self.toast(&reason(&error));
        }
        Some(slug)
    }

    /// Draws an account this dialog just created before the daemon's own status for it
    /// arrives. The daemon persists the account before it answers and publishes its first
    /// status from its own task; until that lands, the account exists only as this row.
    fn note_local_account(&self, provider: &str, account: &str) {
        // Absent only just after an import, when the refreshed catalog is still in
        // flight. The daemon publishes the account's first status either way, so the row
        // appears a moment later rather than not at all.
        let Some(definition) = self.definition(provider) else {
            return;
        };
        if self.status(provider, account).is_none() {
            self.local_added
                .borrow_mut()
                .insert(identity(provider, account));
            self.statuses
                .borrow_mut()
                .push(pending_status(&definition, account));
        }
        self.render();
    }

    /// The label pen on a further account's page. A confirmed rename retires this page —
    /// it is keyed by an identity the rename just replaced — and the dialog returns to
    /// the list when the daemon announces the old id's removal.
    fn rename(self: &Rc<Self>) {
        let Some(((provider, account), _, status)) = self.current() else {
            return;
        };
        let label = status.account_label.unwrap_or_else(|| account.clone());
        let dialog = Rc::clone(self);
        spawn(async move {
            let answer = dialog
                .alerts
                .ask(Question::suggested(
                    "Rename account".into(),
                    "Rename",
                    Content::Name {
                        prefill: label,
                        current: Some(account.clone()),
                    },
                ))
                .await;
            if !answer.confirmed() {
                return;
            }
            let Some(proxy) = dialog.proxy() else {
                return;
            };
            if let Err(error) = proxy
                .rename_account(&provider, &account, &answer.account_id())
                .await
            {
                dialog.toast(&reason(&error));
            }
        });
    }

    fn save_key(self: &Rc<Self>, key: String) {
        let key = key.trim().to_owned();
        let Some(((provider, account), _, _)) = self.current() else {
            return;
        };
        if key.is_empty() {
            return;
        }
        let Some(proxy) = self.proxy() else {
            return;
        };
        let dialog = Rc::clone(self);
        spawn(async move {
            match proxy.set_key(&provider, &account, &key).await {
                Ok(()) => dialog.toast("Key saved. Checking the account…"),
                Err(error) => dialog.toast(&reason(&error)),
            }
        });
    }

    fn sign_out(self: &Rc<Self>, done: &'static str) {
        let Some(((provider, account), _, _)) = self.current() else {
            return;
        };
        let Some(proxy) = self.proxy() else {
            return;
        };
        let dialog = Rc::clone(self);
        spawn(async move {
            match proxy.sign_out(&provider, &account).await {
                Ok(()) => dialog.toast(done),
                Err(error) => dialog.toast(&reason(&error)),
            }
        });
    }

    fn sign_in(self: &Rc<Self>) {
        let Some(((provider, account), _, _)) = self.current() else {
            return;
        };
        let Some(proxy) = self.proxy() else {
            return;
        };
        let dialog = Rc::clone(self);
        spawn(async move {
            let url = match proxy.begin_login(&provider, &account).await {
                Ok(url) => url,
                Err(error) => {
                    dialog.toast(&reason(&error));
                    return;
                }
            };
            // The dialog closed while the login was starting: nobody is left to wait for
            // it, and its loopback listener must not stay bound.
            if !dialog.pending.insert(&provider, &account) {
                let _ = proxy.cancel_login(&provider, &account).await;
                return;
            }
            dialog
                .login_urls
                .borrow_mut()
                .insert(identity(&provider, &account), url.clone());
            dialog.render();

            if let Err(error) = webbrowser::open(&url) {
                tracing::warn!(%error, "could not open the browser");
                dialog.toast("Could not open a browser. Use Copy link and open it yourself.");
            }

            let outcome = proxy.await_login(&provider, &account).await;
            dialog.pending.remove(&provider, &account);
            dialog
                .login_urls
                .borrow_mut()
                .remove(&identity(&provider, &account));
            dialog.render();
            match outcome {
                Ok(()) => dialog.toast("Signed in. Checking the account…"),
                Err(error) => dialog.toast(&reason(&error)),
            }
        });
    }

    fn cancel_login(self: &Rc<Self>) {
        let Some(((provider, account), _, _)) = self.current() else {
            return;
        };
        let Some(proxy) = self.proxy() else {
            return;
        };
        spawn(async move {
            let _ = proxy.cancel_login(&provider, &account).await;
        });
    }

    /// The credential pill. The halves move with the click, before the write lands: the
    /// pill is the one control whose two positions are two screens, and leaving the old
    /// screen under a pill that has already moved reads as the click having missed.
    fn choose_source(self: &Rc<Self>, index: usize) {
        let Some((identity, definition, _)) = self.current() else {
            return;
        };
        let (Some(sources), Some(external)) = (
            detail::source_choices(&definition),
            definition.external.as_ref(),
        ) else {
            return;
        };
        let Some((source, _)) = sources.get(index) else {
            return;
        };
        self.write_option(
            identity,
            SOURCE.to_owned(),
            external.option.clone(),
            source.as_value().to_owned(),
        );
    }

    fn choose_option(self: &Rc<Self>, name: String, index: usize) {
        let Some((identity, definition, status)) = self.current() else {
            return;
        };
        let Some(value) = detail::settings_options(&definition, &status)
            .into_iter()
            .find(|option| option.name == name)
            .and_then(|option| option.choices.get(index))
            .map(|choice| choice.value.clone())
        else {
            return;
        };
        self.write_option(identity, override_key("option", &name), name, value);
    }

    /// Shows the new value at once and writes it; a refusal takes the shown value back.
    fn write_option(self: &Rc<Self>, identity: Identity, key: String, name: String, value: String) {
        let Some(proxy) = self.proxy() else {
            return;
        };
        self.overrides
            .borrow_mut()
            .entry(identity.clone())
            .or_default()
            .insert(key.clone(), value.clone());
        self.render();
        let dialog = Rc::clone(self);
        spawn(async move {
            if let Err(error) = proxy
                .set_option(&identity.0, &identity.1, &name, &value)
                .await
            {
                dialog.forget_override(&identity, &key);
                dialog.toast(&reason(&error));
            }
        });
    }

    fn toggle_notify(self: &Rc<Self>, window: String, enabled: bool) {
        let Some((identity, _, _)) = self.current() else {
            return;
        };
        let Some(proxy) = self.proxy() else {
            return;
        };
        let key = override_key("notify", &window);
        self.overrides
            .borrow_mut()
            .entry(identity.clone())
            .or_default()
            .insert(key.clone(), enabled.to_string());
        self.render();
        let dialog = Rc::clone(self);
        spawn(async move {
            if let Err(error) = proxy
                .set_window_notify(&identity.0, &identity.1, &window, enabled)
                .await
            {
                dialog.forget_override(&identity, &key);
                dialog.toast(&reason(&error));
            }
        });
    }

    fn forget_override(&self, identity: &Identity, key: &str) {
        if let Some(held) = self.overrides.borrow_mut().get_mut(identity) {
            held.remove(key);
        }
        self.render();
    }

    fn recheck(self: &Rc<Self>) {
        let Some(((provider, _), _, _)) = self.current() else {
            return;
        };
        let Some(proxy) = self.proxy() else {
            return;
        };
        let dialog = Rc::clone(self);
        spawn(async move {
            if let Err(error) = proxy.refresh(&provider).await {
                dialog.toast(&reason(&error));
            }
        });
    }

    /// A source row: one Select the daemon validates before storing any of it, so a
    /// refused source leaves the previous one in force and there is nothing to roll back.
    fn choose_candidate(self: &Rc<Self>, id: String) {
        let Some((identity, _, _)) = self.current() else {
            return;
        };
        let Some(selection) = self
            .browser
            .borrow()
            .get(&identity)
            .map(|browser| browser.selection_for(&id))
        else {
            return;
        };
        let Some(proxy) = self.proxy() else {
            return;
        };
        let dialog = Rc::clone(self);
        spawn(async move {
            if let Err(error) = proxy
                .select_auth_source(&identity.0, &identity.1, selection)
                .await
            {
                dialog.toast(&reason(&error));
                return;
            }
            // Publishing now makes the In-use mark arrive with the status rather than at
            // the next scheduled poll.
            if let Err(error) = proxy.refresh(&identity.0).await {
                dialog.toast(&reason(&error));
            }
        });
    }

    /// Nothing is validated on the way in — the daemon stores the header and polls, and
    /// the poll is what says whether the session works.
    fn paste_session(self: &Rc<Self>, session: String) {
        let session = session.trim().to_owned();
        let Some(((provider, account), _, _)) = self.current() else {
            return;
        };
        if session.is_empty() {
            return;
        }
        let Some(proxy) = self.proxy() else {
            return;
        };
        let dialog = Rc::clone(self);
        spawn(async move {
            if let Err(error) = proxy.set_session(&provider, &account, &session).await {
                dialog.toast(&reason(&error));
                return;
            }
            dialog.toast("Session saved. Checking the account…");
            dialog.inspect();
        });
    }

    /// Inspects the page's local sources again. The half goes to its checking note
    /// first, so a slow daemon reads as busy rather than broken; a failure puts back
    /// whatever was there, because refusing to answer is not evidence about any source.
    fn inspect(self: &Rc<Self>) {
        let Some((identity, _, _)) = self.current() else {
            return;
        };
        match self.browser.borrow_mut().get_mut(&identity) {
            Some(browser) => browser.begin_checking(),
            None => return,
        }
        self.render();
        let Some(proxy) = self.proxy() else {
            return;
        };
        let dialog = Rc::clone(self);
        spawn(async move {
            let report = proxy.get_auth_sources(&identity.0, &identity.1).await;
            let failure = {
                let mut browsers = dialog.browser.borrow_mut();
                let Some(browser) = browsers.get_mut(&identity) else {
                    return;
                };
                match report {
                    Ok(report) => {
                        browser.apply_report(report);
                        None
                    }
                    Err(error) => {
                        browser.recover();
                        Some(error)
                    }
                }
            };
            dialog.render();
            if let Some(error) = failure {
                dialog.toast(&reason(&error));
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

/// Whether a provider has a configuration page worth navigating to after it is added.
///
/// A browser-session provider with no options starts polling immediately; an empty page
/// would look like the click did nothing. Choosing among local authentication sources
/// counts as something to configure: which login on this machine to read is the whole
/// decision.
pub(crate) fn opens_detail_after_add(definition: &ProviderDefinition) -> bool {
    definition.browser_auth.is_some()
        || definition.credential != CredentialKind::None.as_wire()
        || !definition.options.is_empty()
}

/// An action shared by a quota card's context menu and its provider settings row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAction {
    AddAccount,
    Modify,
    Remove,
}

impl CardAction {
    const ALL: [Self; 3] = [Self::AddAccount, Self::Modify, Self::Remove];

    pub const fn id(self) -> &'static str {
        match self {
            Self::AddAccount => "add-account",
            Self::Modify => "modify",
            Self::Remove => "remove",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }

    /// What both menus call the action.
    pub const fn label(self) -> &'static str {
        match self {
            Self::AddAccount => "Add new account",
            Self::Modify => "Modify",
            Self::Remove => "Remove",
        }
    }

    /// Removal is the one entry that undoes rather than configures, so the menu draws a
    /// separator above it.
    pub const fn starts_section(self) -> bool {
        matches!(self, Self::Remove)
    }
}

/// The same labels, icons and sections in the card and provider-row menus.
pub(crate) fn menu_entries(actions: impl IntoIterator<Item = CardAction>) -> Vec<MenuEntry> {
    actions
        .into_iter()
        .map(|action| {
            let svg: &[u8] = match action {
                CardAction::AddAccount => include_bytes!("../../ui/icons/list-add-symbolic.svg"),
                CardAction::Modify => include_bytes!("../../ui/icons/document-edit-symbolic.svg"),
                CardAction::Remove => include_bytes!("../../ui/icons/user-trash-symbolic.svg"),
            };
            MenuEntry {
                id: action.id().into(),
                label: action.label().into(),
                icon: slint::Image::load_from_svg_data(svg).unwrap_or_default(),
                has_icon: true,
                section: action.starts_section(),
                enabled: true,
            }
        })
        .collect()
}

/// The entries a quota card's context menu offers for one provider. The same three rules
/// the configured row's menu follows: a second account where the credential can hold
/// one, a pen where there is something to configure, and removal always.
pub fn card_actions(definition: Option<&ProviderDefinition>) -> Vec<CardAction> {
    let mut actions = Vec::new();
    if definition.is_some_and(multi_account_capable) {
        actions.push(CardAction::AddAccount);
    }
    if definition.is_some_and(opens_detail_after_add) {
        actions.push(CardAction::Modify);
    }
    actions.push(CardAction::Remove);
    actions
}

/// Whether a user can give this provider another account: a key or a browser login is
/// something a second account can hold its own copy of, while an external or
/// credential-free provider reads whatever one thing this machine already has.
pub(crate) fn multi_account_capable(definition: &ProviderDefinition) -> bool {
    matches!(
        definition.credential_kind(),
        Some(CredentialKind::Key | CredentialKind::OAuth)
    )
}

/// Whether a typed name can be confirmed: it must suggest an id the config can hold, and
/// a rename must suggest one the account does not already have.
pub fn name_suggests_usable(name: &str, current: Option<&str>) -> bool {
    let id = account_id_suggestion(name);
    valid_account_id(&id) && current.is_none_or(|held| held != id)
}

/// A D-Bus error as one sentence for a toast.
pub(crate) fn reason(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(_, Some(detail), _) => detail.clone(),
        other => other.to_string(),
    }
}

/// Brings a model in line with freshly built rows, touching only rows that changed: a
/// row replaced under the pointer loses the click, and an open menu with it.
fn sync<T: Clone + 'static>(model: &VecModel<T>, rows: Vec<T>, same: impl Fn(&T, &T) -> bool) {
    let held = model.row_count();
    for (index, row) in rows.iter().enumerate() {
        if index >= held {
            model.push(row.clone());
        } else if model.row_data(index).is_none_or(|shown| !same(&shown, row)) {
            model.set_row_data(index, row.clone());
        }
    }
    for index in (rows.len()..held).rev() {
        model.remove(index);
    }
}

/// Images compare by what was loaded, and an empty one is never equal to anything,
/// itself included.
fn same_image(a: &slint::Image, b: &slint::Image) -> bool {
    let empty = |image: &slint::Image| image.size().width == 0;
    (empty(a) && empty(b)) || a == b
}

thread_local! {
    /// What both sides' images are replaced with before the rest of them is compared.
    static BLANK: slint::Image = slint::Image::from_rgba8(slint::SharedPixelBuffer::new(1, 1));
}

fn blank() -> slint::Image {
    BLANK.with(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(credential: CredentialKind) -> ProviderDefinition {
        ProviderDefinition {
            provider: "zai".into(),
            title: "Z.ai".into(),
            credential: credential.as_wire().into(),
            credential_hint: String::new(),
            external: None,
            browser_auth: None,
            options: Vec::new(),
            plugin: None,
        }
    }

    #[test]
    fn a_keyless_provider_without_sources_or_options_returns_to_the_list_after_adding() {
        assert!(!opens_detail_after_add(&definition(CredentialKind::None)));
        assert!(opens_detail_after_add(&definition(CredentialKind::Key)));
    }

    #[test]
    fn a_card_menu_offers_what_the_configured_row_does() {
        use CardAction::{AddAccount, Modify, Remove};
        assert_eq!(
            card_actions(Some(&definition(CredentialKind::Key))),
            [AddAccount, Modify, Remove]
        );
        assert_eq!(
            card_actions(Some(&definition(CredentialKind::External))),
            [Modify, Remove]
        );
        assert_eq!(
            card_actions(Some(&definition(CredentialKind::None))),
            [Remove]
        );
        // A provider the catalog does not know yet can still be removed.
        assert_eq!(card_actions(None), [Remove]);
    }

    #[test]
    fn card_actions_round_trip_through_their_ids() {
        for action in CardAction::ALL {
            assert_eq!(CardAction::from_id(action.id()), Some(action));
        }
        assert_eq!(CardAction::from_id("rename"), None);
    }

    #[test]
    fn key_and_oauth_providers_can_hold_more_than_one_account() {
        assert!(multi_account_capable(&definition(CredentialKind::Key)));
        assert!(multi_account_capable(&definition(CredentialKind::OAuth)));
        assert!(!multi_account_capable(&definition(
            CredentialKind::External
        )));
        assert!(!multi_account_capable(&definition(CredentialKind::None)));
    }

    #[test]
    fn a_name_can_only_be_confirmed_when_it_suggests_a_fresh_id() {
        assert!(!name_suggests_usable("", None));
        assert!(!name_suggests_usable("   ", None));
        assert!(name_suggests_usable("My Work", None));
        assert!(name_suggests_usable("Работа", None));
        assert!(!name_suggests_usable("My Work", Some("My Work")));
        assert!(name_suggests_usable("My Team", Some("My Work")));
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
    fn syncing_touches_only_the_rows_that_changed() {
        let model = VecModel::from(vec![1, 2, 3]);
        sync(&model, vec![1, 5], PartialEq::eq);
        assert_eq!(model.iter().collect::<Vec<_>>(), [1, 5]);
        sync(&model, vec![1, 5, 7], PartialEq::eq);
        assert_eq!(model.iter().collect::<Vec<_>>(), [1, 5, 7]);
    }
}
