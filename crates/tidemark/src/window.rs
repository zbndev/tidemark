//! The window's state and the decisions the GTK `window.rs` makes, over a Slint model.
//!
//! The model keeps one row per account in the order the account was first seen, and never
//! reorders it: each row carries its visible slot instead. A repeater instance therefore
//! stays with its account for the life of the card, so a reorder, an expanded group or a
//! collapsed one is a change of slot, which the markup animates, rather than a change of
//! which data an instance is showing.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::{Rc, Weak};

use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tidemark_types::{DataInfo, Preferences, ProviderDefinition, ProviderStatus, Timestamp, ids};

use crate::alert::{Alerts, Appearance as Look, Content, Question};
use crate::bus::{self, DaemonProxy, Update};
use crate::marks::Marks;
use crate::preferences::PreferencesDialog;
use crate::provider_settings::{self, CardAction, ProviderDialog};
use crate::tray::{self, Tray};
use crate::update::UpdateNotice;
use crate::view::{self, Body, Gauge, Tone};
use crate::{AppWindow, CardData, GaugeData, MenuEntry, RowData, Theme, format, model, update};

/// How often the clock-dependent parts of every card are redrawn.
const TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// One appearance setting as the desktop reports it: the XDG portal on Linux, the
/// registry on Windows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Appearance {
    /// The user prefers a dark style.
    Dark(bool),
    /// sRGB in 0..=1, or `None` when the desktop has no accent to offer.
    Accent(Option<[f64; 3]>),
}

const PAGE_WAITING: i32 = 0;
const PAGE_WELCOME: i32 = 1;
const PAGE_GRID: i32 = 2;

/// Whether the close button hides the window rather than ending the program: only with an
/// icon in the panel to bring it back, and only while the preference asks for it.
fn should_minimize_on_close(tray_available: bool, preference: bool) -> bool {
    tray_available && preference
}

/// Where one account's card goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placement {
    slot: usize,
    shown: bool,
    main: bool,
}

/// Visible slots for statuses held in card order: a provider's first account always, its
/// others only while the group is expanded. A hidden account rests under its provider's
/// card, which is where it slides out from and back to.
fn placements(statuses: &[ProviderStatus], expanded: &BTreeSet<String>) -> Vec<Placement> {
    let mut placements = Vec::with_capacity(statuses.len());
    let mut next = 0;
    for group in model::provider_groups(statuses) {
        let main = next;
        next += 1;
        placements.push(Placement {
            slot: main,
            shown: true,
            main: true,
        });
        let open = expanded.contains(&group[0].provider);
        for _ in &group[1..] {
            placements.push(if open {
                next += 1;
                Placement {
                    slot: next - 1,
                    shown: true,
                    main: false,
                }
            } else {
                Placement {
                    slot: main,
                    shown: false,
                    main: false,
                }
            });
        }
    }
    placements
}

fn identity(status: &ProviderStatus) -> (String, String) {
    (status.provider.clone(), status.account.clone())
}

fn gauge_data(gauge: &Gauge) -> GaugeData {
    GaugeData {
        percent: gauge.percent as f32,
        pace: gauge.pace.map_or(-1.0, |pace| pace as f32),
        tone: match gauge.tone {
            Tone::Normal => 0,
            Tone::Warning => 1,
            Tone::Danger => 2,
        },
        blocked: gauge.blocked,
    }
}

/// Equal by content. `ModelRc` compares by identity, and every redraw builds new rows; an
/// empty `Image` is never equal to anything, itself included, so a card without a mark
/// would always differ.
fn same_card(a: &CardData, b: &CardData) -> bool {
    let placeholder = slint::Image::from_rgba8(slint::SharedPixelBuffer::new(1, 1));
    let rows = |card: &CardData| card.rows.iter().collect::<Vec<RowData>>();
    let bare = |card: &CardData| CardData {
        rows: ModelRc::default(),
        mark: placeholder.clone(),
        ..card.clone()
    };
    bare(a) == bare(b) && rows(a) == rows(b) && (!a.has_mark || a.mark == b.mark)
}

fn account_badge(extra_accounts: usize, expanded: bool) -> String {
    if expanded {
        "−".into()
    } else {
        format!("+{extra_accounts}")
    }
}

pub struct MainWindow {
    ui: slint::Weak<AppWindow>,
    cards: Rc<VecModel<CardData>>,
    /// Which account each model row belongs to, in model order.
    rows: RefCell<Vec<(String, String)>>,
    /// Every account, in card order, provider groups contiguous.
    statuses: RefCell<Vec<ProviderStatus>>,
    /// Provider groups expanded in this window. Deliberately forgotten on the next launch.
    expanded: RefCell<BTreeSet<String>>,
    definitions: RefCell<Vec<ProviderDefinition>>,
    daemon: RefCell<Option<DaemonProxy<'static>>>,
    available: RefCell<String>,
    theme: RefCell<String>,
    system_dark: Cell<bool>,
    marks: Rc<Marks>,
    alerts: Rc<Alerts>,
    /// The open provider dialog, fed everything the daemon says while it is open.
    providers: RefCell<Option<Rc<ProviderDialog>>>,
    /// The account whose card menu is open.
    menu_target: RefCell<Option<(String, String)>>,
    /// What the daemon last said, for the Preferences dialog to open on.
    preferences: RefCell<Preferences>,
    data: RefCell<DataInfo>,
    preferences_dialog: RefCell<Option<Rc<PreferencesDialog>>>,
    /// What the daemon last said its version was, for the About dialog's troubleshooting
    /// page. `None` while nothing is answering on the bus.
    daemon_version: RefCell<Option<String>>,
    /// The renderer the window is drawn with, for the same page.
    renderer: String,
    /// Which daemon releases the restart prompt has already been offered for.
    update_notice: RefCell<UpdateNotice>,
    /// The panel icon, once a status-notifier host has accepted it. `None` in a session
    /// that has none, which is also what leaves the close button closing the program.
    tray: RefCell<Option<Tray>>,
    /// Read at close time, so changing the preference needs no reconnect.
    minimize_on_close: Cell<bool>,
    clock: slint::Timer,
}

impl std::fmt::Debug for MainWindow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MainWindow")
            .field("cards", &self.rows.borrow().len())
            .field("expanded", &self.expanded.borrow())
            .field("connected", &self.daemon.borrow().is_some())
            .finish_non_exhaustive()
    }
}

impl MainWindow {
    /// Builds the window's state and connects it to the daemon. A `background` start is the
    /// session's autostart: the window stays hidden, and the process stays only if a panel
    /// accepts the tray icon, rather than leaving something invisible behind.
    pub fn start(ui: &AppWindow, renderer: String, background: bool) -> Rc<Self> {
        let cards = Rc::new(VecModel::default());
        ui.set_cards(ModelRc::from(Rc::clone(&cards)));
        let main = Rc::new(Self {
            ui: ui.as_weak(),
            cards,
            rows: RefCell::default(),
            statuses: RefCell::default(),
            expanded: RefCell::default(),
            definitions: RefCell::default(),
            daemon: RefCell::default(),
            available: RefCell::default(),
            theme: RefCell::new(Preferences::THEME_SYSTEM.to_owned()),
            system_dark: Cell::new(false),
            marks: Rc::default(),
            alerts: Alerts::install(ui),
            providers: RefCell::default(),
            menu_target: RefCell::default(),
            preferences: RefCell::default(),
            data: RefCell::new(DataInfo {
                config_path: String::new(),
                history_path: String::new(),
                history_bytes: 0,
                key_schema: ids::SECRET_SCHEMA.into(),
                token_schema: ids::TOKEN_SCHEMA.into(),
                release_check_available: false,
                plugin_icons_path: String::new(),
            }),
            preferences_dialog: RefCell::default(),
            daemon_version: RefCell::default(),
            renderer,
            update_notice: RefCell::new(UpdateNotice::new(env!("CARGO_PKG_VERSION"))),
            tray: RefCell::default(),
            minimize_on_close: Cell::new(true),
            clock: slint::Timer::default(),
        });
        ui.set_message("Connecting…".into());

        let weak = Rc::downgrade(&main);
        ui.on_refresh(Self::callback(&weak, |main| main.refresh_now()));
        ui.on_open_release(Self::callback(&weak, |main| main.open_release()));
        ui.on_open_preferences({
            let weak = weak.clone();
            move || {
                if let Some(main) = weak.upgrade() {
                    main.open_preferences();
                }
            }
        });
        ui.on_open_about(Self::callback(&weak, |main| main.open_about()));
        ui.on_open_providers({
            let weak = weak.clone();
            move || {
                if let Some(main) = weak.upgrade() {
                    main.open_providers();
                }
            }
        });
        ui.on_card_menu({
            let weak = weak.clone();
            move |index| {
                let entries = weak
                    .upgrade()
                    .map(|main| main.card_menu(index as usize))
                    .unwrap_or_default();
                ModelRc::new(VecModel::from(entries))
            }
        });
        ui.on_card_action({
            let weak = weak.clone();
            move |id| {
                if let Some(main) = weak.upgrade() {
                    main.card_action(&id);
                }
            }
        });
        ui.on_card_check_error({
            let alerts = Rc::clone(&main.alerts);
            move |detail| {
                let answer = alerts.ask(Question {
                    content: crate::alert::Content::Details,
                    ..Question::notice("Check failed", detail.into())
                });
                spawn(async move {
                    answer.await;
                });
            }
        });
        ui.on_toggle_group({
            let weak = weak.clone();
            move |index| {
                if let Some(main) = weak.upgrade() {
                    main.toggle_group(index as usize);
                }
            }
        });
        ui.on_reorder({
            let weak = weak.clone();
            move |from, to| {
                if let Some(main) = weak.upgrade() {
                    main.reorder(from as usize, to as usize);
                }
            }
        });

        main.clock.start(slint::TimerMode::Repeated, TICK, {
            let weak = weak.clone();
            move || {
                if let Some(main) = weak.upgrade() {
                    main.redraw();
                }
            }
        });

        #[cfg(unix)]
        let watch = crate::portal::watch;
        #[cfg(windows)]
        let watch = crate::registry::watch;
        watch({
            let weak = weak.clone();
            move |appearance| {
                if let Some(main) = weak.upgrade() {
                    main.appearance(appearance);
                }
            }
        });

        ui.window().on_close_requested({
            let weak = weak.clone();
            move || {
                let hide = weak.upgrade().is_some_and(|main| {
                    should_minimize_on_close(
                        main.tray.borrow().is_some(),
                        main.minimize_on_close.get(),
                    )
                });
                if !hide {
                    quit();
                }
                slint::CloseRequestResponse::HideWindow
            }
        });
        main.start_tray(background);

        // The watcher's closure owns the only strong reference, for the life of the process.
        let held = Rc::clone(&main);
        bus::watch(move |update| held.handle(update));
        main
    }

    /// Puts the icon on the panel. Until that has worked the close button ends the
    /// program: hiding a window with nothing left to bring it back is worse than ignoring
    /// the preference.
    fn start_tray(self: &Rc<Self>, background: bool) {
        let weak = Rc::downgrade(self);
        spawn(async move {
            let (commands, inbox) = async_channel::unbounded::<tray::Command>();
            let tray = match Tray::spawn(commands).await {
                Ok(tray) => tray,
                Err(error) => {
                    tracing::info!(
                        %error,
                        "no status-notifier host took the icon; the close button still closes"
                    );
                    if background {
                        quit();
                    }
                    return;
                }
            };
            let Some(main) = weak.upgrade() else {
                return;
            };
            main.tray.replace(Some(tray));
            main.update_tray();
            drop(main);

            while let Ok(command) = inbox.recv().await {
                let Some(main) = weak.upgrade() else {
                    return;
                };
                main.obey(command);
            }
        });
    }

    /// Does what the panel asked for. The tray's own thread only put it on a channel.
    fn obey(&self, command: tray::Command) {
        match command {
            tray::Command::Present => {
                if let Some(ui) = self.ui.upgrade() {
                    present(&ui);
                }
            }
            tray::Command::Refresh => self.refresh_now(),
            tray::Command::Quit => {
                // Taken down here rather than left to the process exit, which on Windows
                // leaves the icon in the notification area until the pointer passes over it.
                self.tray.take();
                quit();
            }
        }
    }

    /// Tells the panel what the window now knows.
    fn update_tray(&self) {
        if let Some(tray) = self.tray.borrow().as_ref() {
            tray.show(
                &self.statuses.borrow(),
                &model::titles(&self.definitions.borrow()),
                self.daemon.borrow().is_some(),
            );
        }
    }

    fn callback(weak: &Weak<Self>, action: impl Fn(&Self) + 'static) -> impl Fn() + 'static {
        let weak = weak.clone();
        move || {
            if let Some(main) = weak.upgrade() {
                action(&main);
            }
        }
    }

    fn handle(&self, update: Update) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        match update {
            Update::Connected {
                proxy,
                version,
                available,
                preferences,
                definitions,
                statuses,
                data,
            } => {
                tracing::info!(accounts = statuses.len(), "connected to the daemon");
                self.daemon.replace(Some(proxy));
                self.offer_restart(&ui, version.as_deref());
                self.daemon_version.replace(version);
                self.definitions.replace(definitions);
                self.marks.set_plugin_root(&data.plugin_icons_path);
                self.data.replace(data);
                ui.set_connected(true);
                self.apply_preferences(&preferences);
                self.show_update(&available);
                self.show_all(statuses);
            }
            Update::Changed(status) => self.show_one(status),
            Update::Removed { provider, account } => self.show_removed(&provider, &account),
            Update::Reordered(providers) => self.show_order(&providers),
            Update::Available(version) => self.show_update(&version),
            Update::Preferences(preferences) => self.apply_preferences(&preferences),
            Update::Data(data) => {
                self.marks.set_plugin_root(&data.plugin_icons_path);
                self.data.replace(data);
                self.update_preferences();
                self.redraw();
            }
            Update::Catalog(definitions) => {
                self.definitions.replace(definitions);
                self.marks.forget();
                self.redraw();
            }
            Update::Activate => present(&ui),
            Update::Waiting(reason) => {
                self.daemon.replace(None);
                self.daemon_version.replace(None);
                ui.set_connected(false);
                self.show_update("");
                ui.set_message(reason.into());
                ui.set_page(PAGE_WAITING);
            }
        }
        self.update_providers();
        self.update_tray();
    }

    /// Opens the one Preferences dialog on what the daemon last said. Like the provider
    /// dialog it asks for the daemon each time it writes.
    fn open_preferences(self: &Rc<Self>) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if self.preferences_dialog.borrow().is_some() || self.daemon.borrow().is_none() {
            return;
        }
        let weak = Rc::downgrade(self);
        let daemon = Rc::new({
            let weak = weak.clone();
            move || weak.upgrade().and_then(|main| main.daemon.borrow().clone())
        });
        let dialog = PreferencesDialog::open(
            &ui,
            daemon,
            Rc::clone(&self.alerts),
            &self.preferences.borrow(),
            &self.data.borrow(),
            move || {
                if let Some(main) = weak.upgrade() {
                    main.preferences_dialog.replace(None);
                }
            },
        );
        self.preferences_dialog.replace(Some(dialog));
    }

    fn update_preferences(&self) {
        let dialog = self.preferences_dialog.borrow().clone();
        if let Some(dialog) = dialog {
            dialog.apply(&self.preferences.borrow(), &self.data.borrow());
        }
    }

    fn open_about(&self) {
        if let Some(ui) = self.ui.upgrade() {
            crate::about::present(
                &ui,
                self.daemon_version.borrow().as_deref(),
                &self.renderer,
                self.tray.borrow().is_some(),
            );
        }
    }

    /// Opens the one provider dialog. It asks for the daemon each time it writes, so a
    /// reconnect underneath it is a new connection rather than a dead one.
    fn open_providers(self: &Rc<Self>) -> Option<Rc<ProviderDialog>> {
        let ui = self.ui.upgrade()?;
        if self.providers.borrow().is_some() || self.daemon.borrow().is_none() {
            return None;
        }
        let weak = Rc::downgrade(self);
        let daemon = Rc::new({
            let weak = weak.clone();
            move || weak.upgrade().and_then(|main| main.daemon.borrow().clone())
        });
        let dialog = ProviderDialog::open(
            &ui,
            daemon,
            Rc::clone(&self.alerts),
            Rc::clone(&self.marks),
            &self.definitions.borrow(),
            &self.statuses.borrow(),
            move || {
                if let Some(main) = weak.upgrade() {
                    main.providers.replace(None);
                }
            },
        );
        self.providers.replace(Some(Rc::clone(&dialog)));
        Some(dialog)
    }

    /// Which entries a card's menu offers, asked as it opens so a catalog that arrived
    /// since the card was drawn is already answered for.
    fn card_menu(&self, row: usize) -> Vec<MenuEntry> {
        let Some((provider, account)) = self.rows.borrow().get(row).cloned() else {
            return Vec::new();
        };
        let actions = provider_settings::card_actions(
            self.definitions
                .borrow()
                .iter()
                .find(|definition| definition.provider == provider),
        );
        self.menu_target.replace(Some((provider, account)));
        provider_settings::menu_entries(actions)
    }

    /// A card's context menu is a shortcut into the provider dialog, never a second way of
    /// doing what it does: the dialog is opened and asked to take the row's own action.
    fn card_action(self: &Rc<Self>, id: &str) {
        let Some((provider, account)) = self.menu_target.take() else {
            return;
        };
        let Some(action) = CardAction::from_id(id) else {
            return;
        };
        if let Some(dialog) = self.open_providers() {
            dialog.shortcut(action, &provider, &account);
        }
    }

    fn update_providers(&self) {
        let dialog = self.providers.borrow().clone();
        if let Some(dialog) = dialog {
            dialog.apply(&self.definitions.borrow(), &self.statuses.borrow());
        }
    }

    /// Replaces everything with what the daemon just said it knows.
    fn show_all(&self, statuses: Vec<ProviderStatus>) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if statuses.is_empty() {
            self.expanded.borrow_mut().clear();
            self.statuses.borrow_mut().clear();
            self.sync_rows();
            ui.set_page(PAGE_WELCOME);
            return;
        }
        self.expanded
            .borrow_mut()
            .retain(|provider| statuses.iter().any(|status| status.provider == *provider));
        let grouped: Vec<ProviderStatus> = model::provider_groups(&statuses)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        self.statuses.replace(grouped);
        self.sync_rows();
        self.redraw();
        ui.set_page(PAGE_GRID);
    }

    /// One account's update. A card for an account seen for the first time opens its group,
    /// so the new account is on screen rather than behind a counter.
    fn show_one(&self, status: ProviderStatus) {
        let known = self
            .statuses
            .borrow()
            .iter()
            .position(|held| identity(held) == identity(&status));
        match known {
            Some(index) => {
                self.statuses.borrow_mut()[index] = status;
                self.redraw();
                if let Some(ui) = self.ui.upgrade() {
                    ui.set_page(PAGE_GRID);
                }
            }
            None => {
                self.expanded.borrow_mut().insert(status.provider.clone());
                let mut statuses = self.statuses.borrow().clone();
                statuses.push(status);
                self.show_all(statuses);
            }
        }
    }

    fn show_removed(&self, provider: &str, account: &str) {
        let mut statuses = self.statuses.borrow().clone();
        statuses.retain(|status| !(status.provider == provider && status.account == account));
        self.show_all(statuses);
    }

    /// Puts the provider groups in the order the daemon published.
    fn show_order(&self, providers: &[String]) {
        let reordered = {
            let statuses = self.statuses.borrow();
            let slugs: Vec<String> = statuses
                .iter()
                .map(|status| status.provider.clone())
                .collect();
            let positions = model::arrangement(&slugs, providers);
            if positions.iter().enumerate().all(|(at, held)| at == *held) {
                return;
            }
            positions
                .into_iter()
                .map(|position| statuses[position].clone())
                .collect()
        };
        self.statuses.replace(reordered);
        self.redraw();
    }

    /// Reorders one provider's accounts without moving any other provider.
    fn show_account_order(&self, provider: &str, accounts: &[String]) {
        {
            let mut statuses = self.statuses.borrow_mut();
            let Some(first) = statuses
                .iter()
                .position(|status| status.provider == provider)
            else {
                return;
            };
            let last = statuses[first..]
                .iter()
                .position(|status| status.provider != provider)
                .map_or(statuses.len(), |offset| first + offset);
            statuses[first..last].sort_by_key(|status| {
                accounts
                    .iter()
                    .position(|account| *account == status.account)
                    .unwrap_or(accounts.len())
            });
        }
        self.redraw();
    }

    /// Gives every account in `statuses` a model row and removes rows for accounts that
    /// have gone. Existing rows keep their index, so their cards keep their instance.
    fn sync_rows(&self) {
        let wanted: Vec<(String, String)> = self.statuses.borrow().iter().map(identity).collect();
        let gone: Vec<usize> = self
            .rows
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, row)| !wanted.contains(row))
            .map(|(index, _)| index)
            .collect();
        for index in gone.into_iter().rev() {
            self.rows.borrow_mut().remove(index);
            self.cards.remove(index);
        }
        for row in wanted {
            if !self.rows.borrow().contains(&row) {
                self.rows.borrow_mut().push(row);
                self.cards.push(CardData::default());
            }
        }
    }

    /// Redraws every card: its reading, its clock-dependent lines, and where it sits.
    fn redraw(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let now = Timestamp::now();
        let statuses = self.statuses.borrow().clone();
        let expanded = self.expanded.borrow().clone();
        let placements = placements(&statuses, &expanded);
        let titles = model::titles(&self.definitions.borrow());

        for (index, (status, placement)) in statuses.iter().zip(&placements).enumerate() {
            let extra_accounts = statuses[index..]
                .iter()
                .skip(1)
                .take_while(|other| other.provider == status.provider)
                .count();
            let badge = if placement.main && extra_accounts > 0 {
                account_badge(extra_accounts, expanded.contains(&status.provider))
            } else {
                String::new()
            };
            let data = self.card_data(status, now, &titles, *placement, badge, &expanded);
            let row = self
                .rows
                .borrow()
                .iter()
                .position(|row| *row == identity(status));
            // Setting a row rebuilds its card, rows and text included; most redraws change
            // nothing, and one landing mid-drag is a dropped frame.
            if let Some(row) = row
                && self
                    .cards
                    .row_data(row)
                    .is_none_or(|shown| !same_card(&shown, &data))
            {
                self.cards.set_row_data(row, data);
            }
        }
        ui.set_shown_count(
            placements
                .iter()
                .filter(|placement| placement.shown)
                .count() as i32,
        );
        self.update_tray();
    }

    fn card_data(
        &self,
        status: &ProviderStatus,
        now: Timestamp,
        titles: &model::Titles,
        placement: Placement,
        badge: String,
        expanded: &BTreeSet<String>,
    ) -> CardData {
        let card = view::card(status, now);
        let mark = self.marks.get(&status.provider);
        let (name, caption) = {
            let provider = model::name(titles, &status.provider);
            if placement.main {
                (provider, String::new())
            } else {
                let account = status.account_label.as_deref().unwrap_or(&status.account);
                (provider, account.to_owned())
            }
        };
        let (chip, chip_tone) = card.chip.map_or((String::new(), 0), |chip| {
            let tone = match chip.tone {
                format::Tone::Neutral => 0,
                format::Tone::Attention => 1,
                format::Tone::Danger => 2,
            };
            (chip.text, tone)
        });

        let mut data = CardData {
            id: format!("{}/{}", status.provider, status.account).into(),
            has_mark: mark.is_some(),
            mark: mark.unwrap_or_default(),
            name: name.into(),
            caption: caption.into(),
            plan: card.plan.unwrap_or_default().into(),
            chip: chip.into(),
            chip_tone,
            footer: card.footer.unwrap_or_default().into(),
            checking: card.checking,
            check_failed: card.check_failed,
            error: status
                .message
                .clone()
                .filter(|message| !message.trim().is_empty())
                .unwrap_or_else(|| status.state.clone())
                .into(),
            slot: placement.slot as i32,
            shown: placement.shown,
            main: placement.main,
            badge: badge.into(),
            expanded: expanded.contains(&status.provider),
            ..CardData::default()
        };
        match card.body {
            Body::Reading(reading) => {
                data.body = 0;
                data.headline = reading.headline.into();
                data.compact = reading.compact;
                data.dominant_title = reading.dominant_title.into();
                data.has_gauge = reading.gauge.is_some();
                data.gauge = reading.gauge.as_ref().map(gauge_data).unwrap_or_default();
                data.blocked = reading.blocked;
                data.reset = reading.reset.unwrap_or_default().into();
                data.absolutes = reading.absolutes.unwrap_or_default().into();
                data.balance = reading.balance.unwrap_or_default().into();
                let rows: Vec<RowData> = reading
                    .rows
                    .iter()
                    .map(|row| RowData {
                        title: row.title.clone().into(),
                        value: row.value.clone().into(),
                        has_gauge: row.gauge.is_some(),
                        gauge: row.gauge.as_ref().map(gauge_data).unwrap_or_default(),
                        blocked: row.blocked,
                    })
                    .collect();
                data.rows = ModelRc::new(VecModel::from(rows));
            }
            Body::Balance { whole, fraction } => {
                data.body = 1;
                data.balance_whole = whole.into();
                data.balance_fraction = fraction.unwrap_or_default().into();
            }
            Body::Blank(message) => {
                data.body = 2;
                data.blank = message.into();
            }
        }
        data
    }

    fn toggle_group(&self, row: usize) {
        let Some(provider) = self
            .rows
            .borrow()
            .get(row)
            .map(|(provider, _)| provider.clone())
        else {
            return;
        };
        {
            let mut expanded = self.expanded.borrow_mut();
            if !expanded.remove(&provider) {
                expanded.insert(provider);
            }
        }
        self.redraw();
    }

    /// A completed drag. Applied here first and sent afterwards, as the GTK window does: the
    /// daemon's echo is a no-op when it agrees, and a refusal puts back what it really has.
    fn reorder(self: &Rc<Self>, from: usize, to: usize) {
        let statuses = self.statuses.borrow().clone();
        let expanded = self.expanded.borrow().clone();
        let placements = placements(&statuses, &expanded);
        let mut visible: Vec<(usize, ProviderStatus)> = statuses
            .iter()
            .zip(&placements)
            .filter(|(_, placement)| placement.shown)
            .map(|(status, placement)| (placement.slot, status.clone()))
            .collect();
        visible.sort_by_key(|(slot, _)| *slot);
        let visible: Vec<ProviderStatus> = visible.into_iter().map(|(_, status)| status).collect();

        let Some(reorder) = model::card_reorder(&statuses, &visible, from, to) else {
            return;
        };
        let Some(proxy) = self.daemon.borrow().clone() else {
            return;
        };
        let weak = Rc::downgrade(self);
        match reorder {
            model::CardReorder::Providers(order) => {
                self.show_order(&order);
                spawn(async move {
                    if let Err(error) = proxy.set_order(&order).await {
                        tracing::warn!(%error, "the daemon refused the new provider order");
                        restore(weak, proxy).await;
                    }
                });
            }
            model::CardReorder::Accounts { provider, accounts } => {
                self.show_account_order(&provider, &accounts);
                spawn(async move {
                    if let Err(error) = proxy.set_account_order(&provider, accounts).await {
                        tracing::warn!(%error, "the daemon refused the new account order");
                        restore(weak, proxy).await;
                    }
                });
            }
        }
    }

    fn apply_preferences(&self, preferences: &Preferences) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        self.minimize_on_close.set(preferences.minimize_on_close);
        self.theme.replace(
            preferences
                .theme
                .clone()
                .unwrap_or_else(|| Preferences::THEME_SYSTEM.to_owned()),
        );
        self.apply_theme();
        let columns_auto = preferences.columns_auto.unwrap_or(true);
        ui.set_max_columns(if columns_auto {
            0
        } else {
            preferences.max_columns.unwrap_or(3) as i32
        });
        self.preferences.replace(preferences.clone());
        self.update_preferences();
    }

    fn appearance(&self, appearance: Appearance) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        match appearance {
            Appearance::Dark(dark) => {
                self.system_dark.set(dark);
                self.apply_theme();
            }
            Appearance::Accent(Some([red, green, blue])) => {
                let channel = |value: f64| (value * 255.0).round() as u8;
                ui.global::<Theme>().set_accent(slint::Color::from_rgb_u8(
                    channel(red),
                    channel(green),
                    channel(blue),
                ));
            }
            Appearance::Accent(None) => {}
        }
    }

    fn apply_theme(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let dark = match self.theme.borrow().as_str() {
            Preferences::THEME_LIGHT => false,
            Preferences::THEME_DARK => true,
            _ => self.system_dark.get(),
        };
        ui.global::<Theme>().set_dark(dark);
    }

    fn show_update(&self, version: &str) {
        self.available.replace(version.to_owned());
        if let Some(ui) = self.ui.upgrade() {
            ui.set_available(version.into());
        }
    }

    /// Previews the release before anyone leaves for a browser: the notes come from the
    /// daemon that found the release, rendered by [`crate::release_notes`].
    ///
    /// A release with no notes — or a daemon that went away, or one too old to know the
    /// call — opens the release page directly, which is all this button ever did before
    /// there was anything to preview.
    fn open_release(&self) {
        let version = self.available.borrow().clone();
        let proxy = self.daemon.borrow().clone();
        let ui = self.ui.clone();
        spawn(async move {
            let notes = match proxy {
                Some(proxy) => proxy.get_release_notes().await.unwrap_or_else(|error| {
                    tracing::info!(
                        %error,
                        "the daemon did not answer GetReleaseNotes; opening the release page"
                    );
                    String::new()
                }),
                None => String::new(),
            };
            if notes.trim().is_empty() {
                let url = update::release_url(&version);
                if let Err(error) = webbrowser::open(&url) {
                    tracing::warn!(%error, "could not open the Tidemark release page");
                }
                return;
            }
            if let Some(ui) = ui.upgrade() {
                crate::release_notes::present(&ui, &version, &notes);
            }
        });
    }

    /// A daemon newer than this window means the package was upgraded underneath it: the
    /// window is brought forward and offers to restart into the new client, once for each
    /// newer release.
    fn offer_restart(&self, ui: &AppWindow, daemon: Option<&str>) {
        let Some(daemon) = daemon else {
            return;
        };
        match self.update_notice.borrow_mut().consider(daemon) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(daemon, %error, "the daemon reported an invalid version");
                return;
            }
        }
        present(ui);
        let alerts = Rc::clone(&self.alerts);
        spawn(async move {
            let answer = alerts
                .ask(Question {
                    heading: "Tidemark has been updated".to_owned(),
                    body: "Restart the app to finish the update.".to_owned(),
                    responses: vec![
                        ("later", "Later", Look::Plain),
                        ("restart", "Restart", Look::Suggested),
                    ],
                    default: "restart",
                    close: "later",
                    content: Content::None,
                })
                .await;
            if answer.response != "restart" {
                return;
            }
            let error = update::restart();
            tracing::error!(%error, "could not restart the desktop client");
            alerts
                .ask(Question::notice(
                    "Tidemark could not restart",
                    format!("Restart the app manually. {error}"),
                ))
                .await;
        });
    }

    fn refresh_now(&self) {
        let Some(proxy) = self.daemon.borrow().clone() else {
            return;
        };
        spawn(async move {
            if let Err(error) = proxy.refresh("").await {
                tracing::warn!(%error, "the daemon refused a refresh");
            }
        });
    }
}

/// Brings the window forward for a second launch.
pub fn present(ui: &AppWindow) {
    let window = ui.window();
    window.set_minimized(false);
    if let Err(error) = window.show() {
        tracing::warn!(%error, "could not bring the window forward");
        return;
    }
    window.with_winit_window(|window| window.focus_window());
}

/// Ends the event loop, which `main` runs until told to so that a hidden window does not end
/// the program.
pub fn quit() {
    if let Err(error) = slint::quit_event_loop() {
        tracing::error!(%error, "the event loop would not stop");
    }
}

pub fn spawn(future: impl std::future::Future<Output = ()> + 'static) {
    if let Err(error) = slint::spawn_local(future) {
        tracing::error!(%error, "the event loop refused a task");
    }
}

/// Puts back the daemon's real order after an optimistic drag was refused.
async fn restore(main: Weak<MainWindow>, proxy: DaemonProxy<'static>) {
    match proxy.get_status().await {
        Ok(statuses) => {
            if let Some(main) = main.upgrade() {
                main.show_all(statuses);
            }
        }
        Err(error) => tracing::warn!(%error, "and did not say what the order actually is"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId};

    fn status(provider: &str, account: &str) -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new(provider), &AccountId::new(account))
    }

    fn card_with_row(value: &str) -> CardData {
        CardData {
            headline: "42%".into(),
            rows: ModelRc::new(VecModel::from(vec![RowData {
                title: "7 days".into(),
                value: value.into(),
                ..RowData::default()
            }])),
            ..CardData::default()
        }
    }

    #[test]
    fn close_hides_only_when_both_the_tray_and_preference_allow_it() {
        assert!(should_minimize_on_close(true, true));
        assert!(!should_minimize_on_close(true, false));
        assert!(!should_minimize_on_close(false, true));
        assert!(!should_minimize_on_close(false, false));
    }

    #[test]
    fn cards_with_equal_rows_in_different_models_are_the_same() {
        assert!(same_card(&card_with_row("2%"), &card_with_row("2%")));
        assert!(!same_card(&card_with_row("2%"), &card_with_row("3%")));
    }

    #[test]
    fn a_collapsed_group_hides_its_accounts_under_the_provider_card() {
        let statuses = [
            status("zai", "default"),
            status("zai", "work"),
            status("claude", "default"),
        ];
        let slots: Vec<(usize, bool)> = placements(&statuses, &BTreeSet::new())
            .iter()
            .map(|placement| (placement.slot, placement.shown))
            .collect();
        assert_eq!(slots, [(0, true), (0, false), (1, true)]);
    }

    #[test]
    fn an_expanded_group_takes_the_slots_after_its_provider() {
        let statuses = [
            status("zai", "default"),
            status("zai", "work"),
            status("claude", "default"),
        ];
        let expanded = BTreeSet::from(["zai".to_owned()]);
        let slots: Vec<(usize, bool)> = placements(&statuses, &expanded)
            .iter()
            .map(|placement| (placement.slot, placement.shown))
            .collect();
        assert_eq!(slots, [(0, true), (1, true), (2, true)]);
    }

    #[test]
    fn the_badge_counts_what_is_hidden_and_offers_to_hide_what_is_shown() {
        assert_eq!(account_badge(2, false), "+2");
        assert_eq!(account_badge(2, true), "−");
    }
}
