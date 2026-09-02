//! The window: a grid of cards, and the two things that can be there instead of it.
//!
//! The cards are in the order the user put them in and nothing else ever changes it: the
//! daemon publishes the sequence, a drag on the grid asks it to publish a different one,
//! and a new account goes on the end. [`crate::grid::CardGrid`] does the columns and the
//! drag; this module owns the vector the sequence applies to and is the only thing that
//! talks to the daemon about it.
//!
//! The window is also where "updates without user action" is made true: statuses arrive on
//! a signal, and a timer redraws the parts that depend on the clock rather than on new
//! data — the relative timestamps, and the pace marks, which keep moving while nothing else
//! changes.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gio, glib};
use tidemark_types::{
    DataInfo, Preferences as AppPreferences, ProviderDefinition, ProviderStatus, Timestamp, ids,
};

use crate::about;
use crate::bus::{self, DaemonProxy, Update};
use crate::card::{Card, CardExpansion, CardTitle};
use crate::detail::DetailDialog;
use crate::grid::CardGrid;
use crate::model;
use crate::preferences::PreferencesDialog;
use crate::provider_settings::ProviderSettings;
use crate::tray::{self, Tray};
use crate::update::{self, UpdateNotice};

/// How often the clock-dependent parts of every card are redrawn. Half a minute is well
/// inside the resolution of everything shown — the coarsest unit on a card is a minute —
/// and costs a redraw of a handful of labels.
const TICK_SECONDS: u32 = 30;

/// Names of the pages in the stack.
const PAGE_GRID: &str = "grid";
const PAGE_MESSAGE: &str = "message";

fn account_index(statuses: &[ProviderStatus], provider: &str, account: &str) -> Option<usize> {
    statuses
        .iter()
        .position(|status| status.provider == provider && status.account == account)
}

fn should_minimize_on_close(tray_available: bool, preference: bool) -> bool {
    tray_available && preference
}

#[derive(Debug)]
struct DialogSlot<T>(RefCell<Option<Rc<T>>>);

impl<T> Default for DialogSlot<T> {
    fn default() -> Self {
        Self(RefCell::new(None))
    }
}

impl<T> DialogSlot<T> {
    fn insert_if_empty(&self, value: Rc<T>) -> bool {
        let mut held = self.0.borrow_mut();
        if held.is_some() {
            return false;
        }
        *held = Some(value);
        true
    }

    fn get(&self) -> Option<Rc<T>> {
        self.0.borrow().clone()
    }

    fn is_empty(&self) -> bool {
        self.0.borrow().is_none()
    }

    fn clear(&self) {
        self.0.borrow_mut().take();
    }
}

/// The main window and everything it is currently showing.
#[derive(Debug)]
pub struct MainWindow {
    window: adw::ApplicationWindow,
    stack: gtk::Stack,
    message: adw::StatusPage,
    grid: CardGrid,
    refresh: gtk::Button,
    release: gtk::Button,
    providers: gtk::Button,
    preferences_action: gio::SimpleAction,
    /// Every account card, ordered by provider and then account. Collapsed account cards stay
    /// here for the tray and provider settings even while the grid omits them.
    cards: RefCell<Vec<Rc<Card>>>,
    /// Provider groups expanded in this window. It deliberately resets on the next launch.
    expanded: RefCell<BTreeSet<String>>,
    definitions: RefCell<Vec<ProviderDefinition>>,
    daemon: RefCell<Option<DaemonProxy<'static>>>,
    /// What the daemon last said its version was, for the About dialog's troubleshooting
    /// page. `None` while nothing is answering on the bus.
    daemon_version: RefCell<Option<String>>,
    update_notice: RefCell<UpdateNotice>,
    preferences: RefCell<AppPreferences>,
    data_info: RefCell<DataInfo>,
    minimize_on_close: Cell<bool>,
    /// The provider dialog while it is open, so live catalog and status changes reach it.
    provider_settings: DialogSlot<ProviderSettings>,
    preferences_dialog: DialogSlot<PreferencesDialog>,
    /// The account detail dialog while it is open, so its chart follows daemon updates.
    detail_dialog: DialogSlot<DetailDialog>,
    /// The panel icon, once a status-notifier host has accepted it. `None` in a session
    /// that has none, which is also what leaves the close button closing the program.
    tray: RefCell<Option<Tray>>,
}

impl MainWindow {
    /// Builds the window, connects it to the daemon and presents it unless this is the
    /// desktop-session autostart. A background start becomes useful only after a panel has
    /// accepted the tray icon; without one it exits rather than leaving an invisible
    /// process behind.
    pub fn present(app: &adw::Application, background: bool) {
        let grid = CardGrid::new();
        grid.set_margin_top(12);
        grid.set_margin_bottom(12);
        grid.set_margin_start(12);
        grid.set_margin_end(12);
        grid.set_valign(gtk::Align::Start);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&grid)
            .build();

        let message = adw::StatusPage::builder()
            .icon_name("network-offline-symbolic")
            .title("Waiting for Tidemark")
            .build();

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        stack.add_named(&scroller, Some(PAGE_GRID));
        stack.add_named(&message, Some(PAGE_MESSAGE));
        stack.set_visible_child_name(PAGE_MESSAGE);

        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Check every provider now")
            .sensitive(false)
            .build();
        let release = gtk::Button::builder()
            .icon_name("software-update-available-symbolic")
            .visible(false)
            .build();
        let providers = gtk::Button::builder()
            .icon_name("dialog-password-symbolic")
            .tooltip_text("Providers and credentials")
            .sensitive(false)
            .build();

        // The platform's primary menu: rightmost in the header, and the place a GNOME user
        // looks for About without being told where it is. `primary` is what makes F10 open
        // it, which is the shortcut the shell's own menus answer to.
        let menu = gio::Menu::new();
        let preferences_section = gio::Menu::new();
        preferences_section.append(Some("_Preferences"), Some("win.preferences"));
        menu.append_section(None, &preferences_section);
        let about_section = gio::Menu::new();
        about_section.append(Some("_About Tidemark"), Some("win.about"));
        menu.append_section(None, &about_section);
        let primary_menu = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main Menu")
            .menu_model(&menu)
            .primary(true)
            .build();

        let header = adw::HeaderBar::new();
        // First packed is furthest right, so the menu leads and the buttons follow it
        // inwards.
        header.pack_end(&primary_menu);
        header.pack_end(&refresh);
        header.pack_end(&release);
        header.pack_start(&providers);

        let view = adw::ToolbarView::builder().content(&stack).build();
        view.add_top_bar(&header);

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Tidemark")
            .default_width(1000)
            .default_height(640)
            .content(&view)
            .build();

        let preferences_action = gio::SimpleAction::new("preferences", None);
        preferences_action.set_enabled(false);

        let main = Rc::new(Self {
            window,
            stack,
            message,
            grid,
            refresh,
            release,
            providers,
            preferences_action,
            cards: RefCell::new(Vec::new()),
            definitions: RefCell::new(Vec::new()),
            daemon: RefCell::new(None),
            daemon_version: RefCell::new(None),
            update_notice: RefCell::new(UpdateNotice::new(env!("CARGO_PKG_VERSION"))),
            preferences: RefCell::new(AppPreferences::default()),
            data_info: RefCell::new(DataInfo {
                config_path: String::new(),
                history_path: String::new(),
                history_bytes: 0,
                key_schema: ids::SECRET_SCHEMA.into(),
                token_schema: ids::TOKEN_SCHEMA.into(),
                release_check_available: false,
            }),
            minimize_on_close: Cell::new(true),
            expanded: RefCell::new(BTreeSet::new()),
            provider_settings: DialogSlot::default(),
            preferences_dialog: DialogSlot::default(),
            detail_dialog: DialogSlot::default(),
            tray: RefCell::new(None),
        });

        main.connect_reorder();
        main.connect_refresh_button();
        main.connect_update_button();
        main.connect_providers_button();
        main.connect_preferences_action();
        main.connect_about_action();
        main.start_clock();
        main.start_tray(background);
        if !background {
            main.window.present();
        }

        // The strong reference lives here, in the closure the bus watcher keeps for the
        // life of the process. GTK owns the widgets, but nothing owns the Rust state that
        // drives them, and a weak reference here would be dead the moment this function
        // returned — leaving a window that connects to the daemon and then ignores every
        // word it says.
        bus::watch(move |update| main.handle(update));
    }

    /// Acts on one message from the daemon.
    fn handle(self: &Rc<Self>, update: Update) {
        match update {
            Update::Connected(
                proxy,
                daemon_version,
                available,
                preferences,
                data,
                definitions,
                statuses,
            ) => {
                *self.daemon.borrow_mut() = Some(proxy);
                self.daemon_version.replace(daemon_version.clone());
                *self.definitions.borrow_mut() = definitions;
                self.preferences_action.set_enabled(true);
                self.refresh.set_sensitive(true);
                self.providers.set_sensitive(true);
                self.apply_preferences(preferences);
                self.apply_data(data);
                self.show_update(&available);
                self.show_all(statuses);

                let offer_restart = daemon_version.as_deref().is_some_and(|version| {
                    match self.update_notice.borrow_mut().consider(version) {
                        Ok(offer) => offer,
                        Err(error) => {
                            tracing::warn!(version, %error, "the daemon reported an invalid version");
                            false
                        }
                    }
                });
                if offer_restart {
                    self.window.present();
                    let parent = self.window.clone();
                    glib::spawn_future_local(async move {
                        let notice = adw::AlertDialog::builder()
                            .heading("Tidemark has been updated")
                            .body("Restart the app to finish the update.")
                            .build();
                        notice.add_responses(&[("later", "Later"), ("restart", "Restart")]);
                        notice.set_default_response(Some("restart"));
                        notice.set_close_response("later");
                        notice
                            .set_response_appearance("restart", adw::ResponseAppearance::Suggested);
                        if notice.choose_future(Some(&parent)).await == "restart" {
                            let error = update::restart();
                            tracing::error!(%error, "could not restart the desktop client");

                            let failure = adw::AlertDialog::builder()
                                .heading("Tidemark could not restart")
                                .body(format!("Restart the app manually. {error}"))
                                .build();
                            failure.add_response("close", "Close");
                            failure.set_close_response("close");
                            failure.choose_future(Some(&parent)).await;
                        }
                    });
                }
            }
            Update::Changed(status) => self.show_one(status),
            Update::Removed { provider, account } => self.show_removed(&provider, &account),
            Update::Reordered(providers) => self.show_order(&providers),
            Update::Available(version) => self.show_update(&version),
            Update::Preferences(preferences) => self.apply_preferences(preferences),
            Update::Data(data) => self.apply_data(data),
            Update::Waiting(reason) => {
                *self.daemon.borrow_mut() = None;
                self.daemon_version.replace(None);
                self.preferences_action.set_enabled(false);
                self.refresh.set_sensitive(false);
                self.providers.set_sensitive(false);
                self.show_update("");
                self.show_message("network-offline-symbolic", "Waiting for Tidemark", &reason);
            }
        }
        // Every path above changed either the readings or whether there are any, and the
        // panel is showing the same thing the grid is. Done here rather than in each arm so
        // that a case added later cannot forget it.
        self.update_tray();
    }

    /// Tells the panel what the window now knows.
    fn update_tray(&self) {
        if let Some(tray) = self.tray.borrow().as_ref() {
            tray.show(
                &self.statuses(),
                &self.titles(),
                self.daemon.borrow().is_some(),
            );
        }
    }

    /// The catalog's spelling of every provider's name, as this client received it.
    fn titles(&self) -> model::Titles {
        model::titles(&self.definitions.borrow())
    }

    /// Replaces everything on screen with what the daemon just said it knows.
    fn show_all(self: &Rc<Self>, statuses: Vec<ProviderStatus>) {
        if statuses.is_empty() {
            // Connected, and there is genuinely nothing to draw. Distinguished from a
            // missing daemon on purpose: one of them is fixed by configuring an account
            // and the other by starting a service.
            self.expanded.borrow_mut().clear();
            self.show_message(
                "view-grid-symbolic",
                "Welcome to Tidemark",
                "Add a provider to start tracking your quota.",
            );
            self.cards.borrow_mut().clear();
            self.grid.clear();
            self.update_provider_settings();
            self.update_detail_from_statuses(&[]);
            return;
        }

        self.expanded
            .borrow_mut()
            .retain(|provider| statuses.iter().any(|status| status.provider == *provider));

        let now = Timestamp::now();
        let mut cards = Vec::new();
        for group in model::provider_groups(&statuses) {
            let provider = &group[0].provider;
            let expanded = self.expanded.borrow().contains(provider);
            let extra_accounts = group.len() - 1;
            for (index, status) in group.iter().enumerate() {
                let title = self.card_title(status, index == 0);
                let expansion = (index == 0 && extra_accounts > 0).then(|| {
                    let weak = Rc::downgrade(self);
                    let provider = provider.clone();
                    CardExpansion {
                        extra_accounts,
                        expanded,
                        on_toggled: Rc::new(move |expanded| {
                            if let Some(main) = weak.upgrade() {
                                main.set_group_expanded(&provider, expanded);
                            }
                        }),
                    }
                });
                cards.push(self.make_card(status, now, title, expansion));
            }
        }
        *self.cards.borrow_mut() = cards;
        self.redraw_cards();
        self.stack.set_visible_child_name(PAGE_GRID);
        self.update_provider_settings();
        self.update_detail_from_statuses(&statuses);
    }

    /// Removes one account's card after the daemon confirms it is no longer configured.
    fn show_removed(self: &Rc<Self>, provider: &str, account: &str) {
        if let Some(dialog) = self.detail_dialog.get()
            && dialog.matches(provider, account)
        {
            dialog.close();
            self.detail_dialog.clear();
        }
        let index = account_index(&self.statuses(), provider, account);
        let Some(index) = index else {
            return;
        };

        self.cards.borrow_mut().remove(index);
        self.show_all(self.statuses());
    }

    /// Applies one account's update, adding a card for an account seen for the first time.
    fn show_one(self: &Rc<Self>, status: ProviderStatus) {
        let now = Timestamp::now();
        let existing = self.cards.borrow().iter().position(|card| {
            let held = card.status();
            held.provider == status.provider && held.account == status.account
        });

        match existing {
            Some(index) => {
                let card = Rc::clone(&self.cards.borrow()[index]);
                card.set_title(self.card_title(&status, status.account == "default"));
                card.apply(&status, now);
            }
            None => {
                self.expanded.borrow_mut().insert(status.provider.clone());
                let mut statuses = self.statuses();
                statuses.push(status);
                self.show_all(statuses);
                return;
            }
        }
        self.stack.set_visible_child_name(PAGE_GRID);
        self.update_provider_settings();
        if let Some(dialog) = self.detail_dialog.get()
            && dialog.matches(&status.provider, &status.account)
        {
            dialog.apply(&status);
        }
    }

    /// Everything the daemon has said, in the order the cards are on screen.
    fn statuses(&self) -> Vec<ProviderStatus> {
        self.cards
            .borrow()
            .iter()
            .map(|card| card.status())
            .collect()
    }

    /// Feeds the open provider dialog without consulting widget visibility for ownership.
    fn update_provider_settings(&self) {
        if let Some(dialog) = self.provider_settings.get() {
            dialog.apply(&self.definitions.borrow(), &self.statuses());
        }
    }

    /// Replaces the visible subset of account cards without affecting the tray's full list.
    fn redraw_cards(&self) {
        self.grid.replace(
            self.visible_cards()
                .into_iter()
                .map(|card| card.widget().clone())
                .collect(),
        );
    }

    /// The cards the grid currently owns: main cards always, extra cards only when expanded.
    fn visible_cards(&self) -> Vec<Rc<Card>> {
        let cards = self.cards.borrow();
        let mut visible = Vec::new();
        let mut first = 0;
        while first < cards.len() {
            let provider = cards[first].status().provider;
            let expanded = self.expanded.borrow().contains(&provider);
            let mut next = first + 1;
            while next < cards.len() && cards[next].status().provider == provider {
                next += 1;
            }
            visible.push(Rc::clone(&cards[first]));
            if expanded {
                visible.extend(cards[first + 1..next].iter().cloned());
            }
            first = next;
        }
        visible
    }

    /// Stores an in-memory expansion choice and redraws just the grid representation.
    fn set_group_expanded(&self, provider: &str, expanded: bool) {
        if expanded {
            self.expanded.borrow_mut().insert(provider.into());
        } else {
            self.expanded.borrow_mut().remove(provider);
        }
        self.redraw_cards();
    }

    /// The title a card shows: provider title for the main account, account label otherwise.
    fn card_title(&self, status: &ProviderStatus, main: bool) -> CardTitle {
        let provider = model::name(&self.titles(), &status.provider);
        if main {
            CardTitle::main(provider)
        } else {
            let account = status.account_label.as_deref().unwrap_or(&status.account);
            CardTitle::child(&provider, account)
        }
    }

    /// Builds a card whose activation is owned by this window, not by the card itself.
    fn make_card(
        self: &Rc<Self>,
        status: &ProviderStatus,
        now: Timestamp,
        title: CardTitle,
        expansion: Option<CardExpansion>,
    ) -> Rc<Card> {
        let weak = Rc::downgrade(self);
        Rc::new(Card::new(
            status,
            now,
            title,
            Rc::new(move |provider, account| {
                if let Some(main) = weak.upgrade() {
                    main.open_detail(&provider, &account);
                }
            }),
            expansion,
        ))
    }

    /// Opens one dimmed detail dialog and refuses a second until the first closes.
    fn open_detail(self: &Rc<Self>, provider: &str, account: &str) {
        if !self.detail_dialog.is_empty() {
            return;
        }
        let Some(proxy) = self.daemon.borrow().clone() else {
            return;
        };
        let Some(status) = self
            .cards
            .borrow()
            .iter()
            .map(|card| card.status())
            .find(|status| status.provider == provider && status.account == account)
        else {
            return;
        };
        let weak = Rc::downgrade(self);
        let provider_name = model::name(&self.titles(), &status.provider);
        let dialog = DetailDialog::present(&self.window, proxy, status, provider_name, move || {
            if let Some(main) = weak.upgrade() {
                main.detail_dialog.clear();
            }
        });
        assert!(self.detail_dialog.insert_if_empty(dialog));
    }

    /// Keeps the detail dialog honest after a full status reload, including daemon restart.
    fn update_detail_from_statuses(&self, statuses: &[ProviderStatus]) {
        let Some(dialog) = self.detail_dialog.get() else {
            return;
        };
        if let Some(status) = statuses
            .iter()
            .find(|status| dialog.matches(&status.provider, &status.account))
        {
            dialog.apply(status);
        } else {
            dialog.close();
            self.detail_dialog.clear();
        }
    }

    fn connect_providers_button(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        self.providers.connect_clicked(move |_| {
            let Some(main) = weak.upgrade() else {
                return;
            };
            if !main.provider_settings.is_empty() {
                return;
            }
            let Some(proxy) = main.daemon.borrow().clone() else {
                return;
            };
            let on_closed = {
                let weak = weak.clone();
                move || {
                    if let Some(main) = weak.upgrade() {
                        main.provider_settings.clear();
                    }
                }
            };
            let dialog = ProviderSettings::present(
                &main.window,
                proxy,
                &main.definitions.borrow(),
                &main.statuses(),
                on_closed,
            );
            assert!(main.provider_settings.insert_if_empty(dialog));
        });
    }

    fn connect_preferences_action(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        self.preferences_action.connect_activate(move |_, _| {
            let Some(main) = weak.upgrade() else {
                return;
            };
            if let Some(dialog) = main.preferences_dialog.get() {
                dialog.apply(&main.preferences.borrow(), &main.data_info.borrow());
                return;
            }
            let Some(proxy) = main.daemon.borrow().clone() else {
                return;
            };
            let on_closed = {
                let weak = weak.clone();
                move || {
                    if let Some(main) = weak.upgrade() {
                        main.preferences_dialog.clear();
                    }
                }
            };
            let dialog = PreferencesDialog::present(
                &main.window,
                proxy,
                main.preferences.borrow().clone(),
                main.data_info.borrow().clone(),
                on_closed,
            );
            assert!(main.preferences_dialog.insert_if_empty(dialog));
        });
        self.window.add_action(&self.preferences_action);
    }

    fn apply_preferences(&self, preferences: AppPreferences) {
        self.minimize_on_close.set(preferences.minimize_on_close);
        *self.preferences.borrow_mut() = preferences;
        if let Some(dialog) = self.preferences_dialog.get() {
            dialog.apply(&self.preferences.borrow(), &self.data_info.borrow());
        }
    }

    fn apply_data(&self, data: DataInfo) {
        *self.data_info.borrow_mut() = data;
        if let Some(dialog) = self.preferences_dialog.get() {
            dialog.apply(&self.preferences.borrow(), &self.data_info.borrow());
        }
    }

    /// Installs `win.about`, the one entry in the primary menu.
    ///
    /// A window action rather than an application one: what the dialog reports — which
    /// daemon answered, whether the panel took the icon — is this window's knowledge, and
    /// an `app.` action would have to go looking for the window to find it.
    fn connect_about_action(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        let action = gio::SimpleAction::new("about", None);
        action.connect_activate(move |_, _| {
            let Some(main) = weak.upgrade() else {
                return;
            };
            let info = about::debug_info(
                main.daemon_version.borrow().as_deref(),
                main.tray.borrow().is_some(),
            );
            about::present(&main.window, &info);
        });
        self.window.add_action(&action);
    }

    /// Mirrors a completed drag into the card vector and asks the daemon to keep it.
    ///
    /// The move is applied here first and sent afterwards. A grid that waited for a round
    /// trip before showing where the card landed would feel broken on a loaded machine, and
    /// the daemon's answer changes nothing when it succeeds: it echoes the same sequence
    /// back as `OrderChanged`, which [`MainWindow::show_order`] recognises as a no-op.
    ///
    /// A refusal is the interesting case. It means the configured set moved underneath the
    /// drag — an account added or removed while it was in the air — so the cards go back to
    /// what the daemon actually has rather than staying somewhere nobody asked for.
    fn connect_reorder(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        self.grid.connect_reordered(move |from, to| {
            let Some(main) = weak.upgrade() else {
                return;
            };
            let statuses = main.statuses();
            let visible: Vec<ProviderStatus> = main
                .visible_cards()
                .iter()
                .map(|card| card.status())
                .collect();
            let Some(reorder) = model::card_reorder(&statuses, &visible, from, to) else {
                main.redraw_cards();
                return;
            };
            let Some(proxy) = main.daemon.borrow().clone() else {
                main.redraw_cards();
                return;
            };
            match reorder {
                model::CardReorder::Providers(order) => {
                    main.show_order(&order);
                    let weak = Rc::downgrade(&main);
                    glib::spawn_future_local(async move {
                        if let Err(error) = proxy.set_order(&order).await {
                            tracing::warn!(%error, "the daemon refused the new provider order");
                            MainWindow::restore_order_after_refusal(weak, proxy).await;
                        }
                    });
                }
                model::CardReorder::Accounts { provider, accounts } => {
                    main.show_account_order(&provider, &accounts);
                    let weak = Rc::downgrade(&main);
                    glib::spawn_future_local(async move {
                        if let Err(error) = proxy.set_account_order(&provider, accounts).await {
                            tracing::warn!(%error, "the daemon refused the new account order");
                            MainWindow::restore_order_after_refusal(weak, proxy).await;
                        }
                    });
                }
            }
        });
    }

    /// Restores the full daemon order after an optimistic drag was rejected.
    async fn restore_order_after_refusal(weak: Weak<Self>, proxy: DaemonProxy<'static>) {
        match proxy.get_status().await {
            Ok(statuses) => {
                if let Some(main) = weak.upgrade() {
                    main.show_all(statuses);
                }
            }
            Err(error) => tracing::warn!(%error, "and did not say what the order actually is"),
        }
    }

    /// Puts the cards in the order the daemon published.
    fn show_order(&self, providers: &[String]) {
        let slugs: Vec<String> = self
            .cards
            .borrow()
            .iter()
            .map(|card| card.status().provider)
            .collect();
        let positions = model::arrangement(&slugs, providers);
        if positions.iter().enumerate().all(|(at, held)| at == *held) {
            return;
        }
        {
            let mut cards = self.cards.borrow_mut();
            let mut held: Vec<Option<Rc<Card>>> = cards.drain(..).map(Some).collect();
            for position in positions {
                if let Some(card) = held[position].take() {
                    cards.push(card);
                }
            }
        }
        self.redraw_cards();
        self.update_provider_settings();
        self.update_tray();
    }

    /// Reorders one provider's contiguous account group without moving any other provider.
    fn show_account_order(&self, provider: &str, accounts: &[String]) {
        let mut cards = self.cards.borrow_mut();
        let Some(first) = cards
            .iter()
            .position(|card| card.status().provider == provider)
        else {
            return;
        };
        let last = cards[first..]
            .iter()
            .position(|card| card.status().provider != provider)
            .map_or(cards.len(), |offset| first + offset);
        let mut group: Vec<Rc<Card>> = cards.drain(first..last).collect();
        group.sort_by_key(|card| {
            accounts
                .iter()
                .position(|account| *account == card.status().account)
                .unwrap_or(accounts.len())
        });
        cards.splice(first..first, group);
        drop(cards);
        self.redraw_cards();
        self.update_provider_settings();
        self.update_tray();
    }

    /// Shows the message page instead of the grid.
    fn show_message(&self, icon: &str, title: &str, reason: &str) {
        self.message.set_icon_name(Some(icon));
        self.message.set_title(title);
        self.message.set_description(Some(reason));
        self.stack.set_visible_child_name(PAGE_MESSAGE);
    }

    fn connect_refresh_button(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        self.refresh.connect_clicked(move |_| {
            if let Some(main) = weak.upgrade() {
                main.refresh_now();
            }
        });
    }

    /// Opens the fixed release list; the daemon-provided value controls visibility only.
    fn connect_update_button(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        self.release.connect_clicked(move |_| {
            let Some(main) = weak.upgrade() else {
                return;
            };
            let parent = main.window.clone();
            glib::spawn_future_local(async move {
                let launcher = gtk::UriLauncher::new(update::RELEASES_URL);
                if let Err(error) = launcher.launch_future(Some(&parent)).await {
                    tracing::warn!(%error, "could not open the Tidemark releases page");
                }
            });
        });
    }

    fn show_update(&self, version: &str) {
        let tooltip = update::update_tooltip(version);
        self.release.set_tooltip_text(tooltip.as_deref());
        self.release.set_visible(tooltip.is_some());
    }

    /// Polls every account now. The header button and the tray menu both mean this, and
    /// they call it rather than each building the request, so the two cannot come to mean
    /// different things.
    fn refresh_now(&self) {
        let Some(proxy) = self.daemon.borrow().clone() else {
            return;
        };
        glib::spawn_future_local(async move {
            // An empty slug means every account. The daemon re-reads the credentials as
            // part of this, which is why the button is worth having at all: it is the
            // recovery path after unlocking a keyring or storing a key.
            if let Err(error) = proxy.refresh("").await {
                tracing::warn!(%error, "the daemon refused a refresh");
            }
        });
    }

    /// Puts the icon on the panel, and — only if that worked — makes the close button
    /// hide the window instead of ending the program.
    ///
    /// The order matters and is the whole reason this is one function. Closing to a tray
    /// that is not there leaves a running process with no window and no way to ask for one
    /// back, so the close handler is connected inside the success branch and nowhere else.
    /// A session with no status-notifier host therefore behaves exactly as it did before
    /// this existed: the close button closes.
    fn start_tray(self: &Rc<Self>, background: bool) {
        let weak: Weak<Self> = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let (commands, inbox) = async_channel::unbounded::<tray::Command>();
            let tray = match Tray::spawn(commands).await {
                Ok(tray) => tray,
                Err(error) => {
                    tracing::info!(
                        %error,
                        "no status-notifier host took the icon; the close button still closes"
                    );
                    if background
                        && let Some(main) = weak.upgrade()
                        && let Some(app) = main.window.application()
                    {
                        app.quit();
                    }
                    return;
                }
            };

            let Some(main) = weak.upgrade() else {
                return;
            };
            *main.tray.borrow_mut() = Some(tray);
            main.update_tray();
            main.close_to_tray();
            drop(main);

            while let Ok(command) = inbox.recv().await {
                let Some(main) = weak.upgrade() else {
                    return;
                };
                main.obey(command);
            }
        });
    }

    /// Turns the close button into a hide, now that there is an icon to get back from.
    fn close_to_tray(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |window| {
            let Some(main) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if !should_minimize_on_close(main.tray.borrow().is_some(), main.minimize_on_close.get())
            {
                return glib::Propagation::Proceed;
            }
            window.set_visible(false);
            // A working tray is already guaranteed by the only caller. The preference is
            // read at close time, so changing it needs no reconnect or restart.
            glib::Propagation::Stop
        });
    }

    /// Does what the panel asked for. Runs on the main thread; the tray's own thread only
    /// ever put a value on a channel.
    fn obey(&self, command: tray::Command) {
        match command {
            tray::Command::Present => self.window.present(),
            tray::Command::Refresh => self.refresh_now(),
            tray::Command::Quit => {
                if let Some(app) = self.window.application() {
                    app.quit();
                }
            }
        }
    }

    /// Starts the redraw that keeps "resets in 3 h" and the pace marks honest.
    fn start_clock(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        glib::timeout_add_seconds_local(TICK_SECONDS, move || {
            let Some(main) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let now = Timestamp::now();
            for card in main.cards.borrow().iter() {
                card.retime(now);
            }
            glib::ControlFlow::Continue
        });
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use tidemark_types::{AccountId, ProviderId, ProviderStatus};

    use super::{DialogSlot, account_index, should_minimize_on_close};

    fn status(provider: &str, account: &str) -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new(provider), &AccountId::new(account))
    }

    #[test]
    fn removal_matches_the_full_provider_account_identity() {
        let statuses = vec![status("zai", "first"), status("zai", "default")];
        assert_eq!(account_index(&statuses, "zai", "default"), Some(1));
        assert_eq!(account_index(&statuses, "kimi", "default"), None);
    }

    #[test]
    fn a_closed_dialog_slot_can_be_filled_again() {
        let slot = DialogSlot::default();
        assert!(slot.insert_if_empty(Rc::new(1)));
        assert!(!slot.insert_if_empty(Rc::new(2)));
        slot.clear();
        assert!(slot.insert_if_empty(Rc::new(3)));
    }
    #[test]
    fn close_hides_only_when_both_the_tray_and_preference_allow_it() {
        assert!(should_minimize_on_close(true, true));
        assert!(!should_minimize_on_close(true, false));
        assert!(!should_minimize_on_close(false, true));
    }
}
