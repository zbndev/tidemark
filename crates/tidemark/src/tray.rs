//! The status-notifier icon, and the menu behind it.
//!
//! `CONTEXT.md` § Interface: a static icon whose menu lists the configured accounts with
//! the percentage of their shortest window. It is the program's minimised form — closing
//! the window hides it and leaves this behind, and the only way out is the menu's Quit.
//!
//! # Why ksni and not GDBus by hand
//!
//! The plan's original instruction was to speak StatusNotifierItem and
//! `com.canonical.dbusmenu` directly, because `libayatana-appindicator-glib` is GPL-3 and
//! cannot be linked into an MIT project. `ksni` is the third option and the one taken:
//! it is **Unlicense** — public domain, compatible with anything — and it is built on the
//! same zbus 5 with the same async-io backend this crate already uses to reach the daemon.
//! Hand-rolling `com.canonical.dbusmenu` would have been several hundred lines of protocol
//! for no licence benefit.
//!
//! # Which thread runs what
//!
//! ksni drives its own connection on an executor thread it owns, so every method of
//! [`Model`] — including the menu callbacks — runs *off* the GTK main thread. Nothing here
//! touches a widget as a result: a callback puts a [`Command`] on a channel and returns,
//! and [`Tray::spawn`] leaves a task on the main context that receives them and acts. That
//! is also why [`Model`] holds published statuses rather than a handle to the window.
//!
//! # The one part that is a pure function
//!
//! [`entries`] turns what the daemon published into the lines of the menu, and reaches for
//! neither the clock nor the display, so the cases worth checking — an account that has
//! never answered, two accounts of one provider, a rejected key — are tested here rather
//! than by opening a menu and looking at it.

use gtk::glib;
use ksni::TrayMethods;
use tidemark_types::{DANGER_AT, ProviderStatus, ids, present};

use crate::format;
use crate::model;

/// The icon the panel shows. It deliberately uses the same full-colour icon name as the
/// application: `data/icons` supplies native small sizes so a panel never has to enlarge a
/// tiny fallback pixmap, and the `PKGBUILD` installs them all.
const ICON: &str = ids::APP_ID;

/// What a menu row asks the interface to do.
///
/// Deliberately tiny and free of widgets: these cross a thread boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Show the window, whether it is hidden or merely behind something.
    Present,
    /// Poll every provider now — the header bar's refresh button, from the panel.
    Refresh,
    /// Leave. The only way out once the window closes to the tray instead of exiting.
    Quit,
}

/// One account, as the menu says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Provider name, with the account after it when another entry shares the provider.
    pub label: String,
    /// The right-hand half: a percentage, or why there is not one.
    pub value: String,
}

impl Entry {
    /// The single string a `com.canonical.dbusmenu` row can carry.
    ///
    /// One label per row is all the protocol has — there is no second column to align a
    /// percentage in — so the two halves are joined here rather than in the menu builder,
    /// where a test could not see them.
    pub fn line(&self) -> String {
        format!("{} — {}", self.label, self.value)
    }
}

/// The accounts as the menu lists them: the order the grid is in, which is the order the
/// user dragged the cards into.
///
/// It sorts nothing. The window hands over its cards in the order they are on screen, and a
/// panel that applied a rule of its own would be a second opinion about an order the user
/// set by hand. The percentage is of [`tidemark_types::Snapshot::dominant_window`] — the
/// shortest window — which is the same rule the card leads with.
///
/// An account whose last poll did not produce a reading says what [`format::chip`] says on
/// its card, so the two never spell one situation two ways.
pub fn entries(statuses: &[ProviderStatus], titles: &model::Titles) -> Vec<Entry> {
    statuses
        .iter()
        .map(|status| Entry {
            label: label(statuses, status, titles),
            value: value(status),
        })
        .collect()
}

/// Whether anything is close enough to its limit for the panel to highlight the icon.
///
/// [`DANGER_AT`] rather than a number of its own: the bar changes colour here and the
/// notification fires here, and a tray that picked its own threshold would be a third
/// opinion about when a window became worth worrying about.
pub fn needs_attention(statuses: &[ProviderStatus]) -> bool {
    statuses.iter().any(|status| {
        status
            .to_snapshot()
            .is_some_and(|snapshot| snapshot.windows.iter().any(|w| w.used_percent >= DANGER_AT))
    })
}

/// The provider's name, with the account after it only when it is needed to tell two rows
/// apart. One account per provider is the ordinary case and `Claude (default)` would be a
/// word of noise on every line of the menu.
fn label(all: &[ProviderStatus], status: &ProviderStatus, titles: &model::Titles) -> String {
    let name = model::name(titles, &status.provider);
    let shared = all
        .iter()
        .filter(|other| other.provider == status.provider)
        .count()
        > 1;
    if shared {
        format!("{name} ({})", status.account)
    } else {
        name
    }
}

/// The right-hand half of a row: how full the shortest window is, or what is in the way.
///
/// A reading survives a failed poll — `ProviderStatus::windows` keeps the last good one —
/// so a rate-limited account that has numbers shows them, and the chip is what says the
/// numbers are not fresh. Only an account with no reading at all falls back to the chip.
fn value(status: &ProviderStatus) -> String {
    let dominant = status
        .to_snapshot()
        .and_then(|snapshot| snapshot.dominant_window().map(|window| window.used_percent));
    match dominant {
        Some(used) => present::percent(used),
        None => format::chip(status)
            .map(|chip| chip.text)
            .unwrap_or_else(|| "no reading".to_owned()),
    }
}

/// Everything the menu needs, computed on the GTK thread and shipped to ksni's.
///
/// The interface does the deciding and the tray only stores the answer, so that the pure
/// functions above stay the single place any of this is worked out — and so that nothing
/// crossing the thread boundary has to be more than plain data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// The account rows, in the order the grid uses.
    pub entries: Vec<Entry>,
    /// Whether the panel should highlight the icon.
    pub attention: bool,
    /// Whether the daemon is answering. Drives what the menu says instead of accounts, and
    /// whether refreshing is offered at all.
    pub connected: bool,
}

impl State {
    /// What the interface currently knows, as the tray needs it.
    pub fn of(statuses: &[ProviderStatus], titles: &model::Titles, connected: bool) -> Self {
        Self {
            entries: entries(statuses, titles),
            attention: needs_attention(statuses),
            connected,
        }
    }
}

/// The `com.canonical.dbusmenu` item, as ksni drives it.
#[derive(Debug)]
pub struct Model {
    state: State,
    commands: async_channel::Sender<Command>,
}

impl Model {
    /// The row shown in place of the accounts, or `None` when there are accounts to show.
    ///
    /// Three situations the window already distinguishes, kept distinct here for the same
    /// reason: one is fixed by starting a service and another by adding a provider.
    fn placeholder(&self) -> Option<&'static str> {
        match (self.state.connected, self.state.entries.is_empty()) {
            (false, _) => Some("Waiting for Tidemark…"),
            (true, true) => Some("No providers configured"),
            (true, false) => None,
        }
    }

    /// A row that says something rather than doing something.
    fn caption(label: &str) -> ksni::MenuItem<Self> {
        ksni::menu::StandardItem {
            label: mnemonics(label),
            enabled: false,
            ..Default::default()
        }
        .into()
    }

    /// A row that sends `command` and returns immediately, as ksni asks: this runs on the
    /// tray's own thread, and everything it could actually do lives on the GTK one.
    fn action(label: &str, icon: &str, enabled: bool, command: Command) -> ksni::MenuItem<Self> {
        ksni::menu::StandardItem {
            label: mnemonics(label),
            icon_name: icon.to_owned(),
            enabled,
            activate: Box::new(move |this: &mut Self| this.send(command)),
            ..Default::default()
        }
        .into()
    }

    /// Hands a command to the interface. Never blocks and never panics: the channel is
    /// unbounded, and a closed one means the window is already going away.
    fn send(&self, command: Command) {
        if self.commands.try_send(command).is_err() {
            tracing::debug!(?command, "the interface is no longer listening to the tray");
        }
    }
}

impl ksni::Tray for Model {
    fn id(&self) -> String {
        ids::APP_ID.to_owned()
    }

    fn title(&self) -> String {
        "Tidemark".to_owned()
    }

    fn icon_name(&self) -> String {
        ICON.to_owned()
    }

    fn attention_icon_name(&self) -> String {
        ICON.to_owned()
    }

    fn status(&self) -> ksni::Status {
        if self.state.attention {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    /// A left click shows the window. That is the whole of what a tray icon is for here,
    /// and the menu is the right button, which is where a panel puts it anyway.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::Present);
    }

    /// The one line a panel shows on hover: the account nearest its limit, which is the
    /// first row for the same reason it is the first card.
    fn tool_tip(&self) -> ksni::ToolTip {
        let description = match self.placeholder() {
            Some(reason) => reason.to_owned(),
            None => self.state.entries[0].line(),
        };
        ksni::ToolTip {
            icon_name: ICON.to_owned(),
            title: "Tidemark".to_owned(),
            description,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let mut items = match self.placeholder() {
            Some(reason) => vec![Self::caption(reason)],
            None => self
                .state
                .entries
                .iter()
                // An account row shows the window rather than being dead text: the panel
                // is where the user noticed the number, and the window is where they can
                // do anything about it.
                .map(|entry| Self::action(&entry.line(), "", true, Command::Present))
                .collect(),
        };

        items.push(ksni::MenuItem::Separator);
        items.push(Self::action(
            "Open Tidemark",
            "window-new-symbolic",
            true,
            Command::Present,
        ));
        items.push(Self::action(
            "Refresh now",
            "view-refresh-symbolic",
            self.state.connected,
            Command::Refresh,
        ));
        items.push(ksni::MenuItem::Separator);
        items.push(Self::action(
            "Quit",
            "application-exit-symbolic",
            true,
            Command::Quit,
        ));
        items
    }

    fn watcher_online(&self) {
        tracing::info!("a status-notifier watcher is on the bus");
    }

    /// Keep the item alive and wait: a shell being restarted takes its watcher with it, and
    /// giving up would leave a window that can only be closed, never reopened.
    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        tracing::info!(
            ?reason,
            "the status-notifier watcher went away; waiting for it"
        );
        true
    }
}

/// Escapes a label for `com.canonical.dbusmenu`, which reads a single underscore as the
/// marker before an access key and swallows it.
///
/// Not hypothetical: account slugs come from the user's `config.toml`, so an account called
/// `work_key` would otherwise appear in the panel as `workkey` with a mnemonic on the `k`.
fn mnemonics(label: &str) -> String {
    label.replace('_', "__")
}

/// The tray, from the interface's side.
///
/// Owning it keeps the icon up; dropping it takes the icon down, which is what makes the
/// window's lifetime and the icon's the same thing.
#[derive(Debug)]
pub struct Tray {
    outbox: async_channel::Sender<State>,
}

impl Tray {
    /// Puts the icon on the panel, or explains why it could not be done.
    ///
    /// **A failure here is not fatal and must not be treated as one.** It means this
    /// session has no status-notifier host, and the caller's job is then to leave the
    /// window closing the way it always did — hiding it with nothing to bring it back is
    /// the one outcome worse than having no tray.
    ///
    /// `commands` receives what the user picked; it is drained on the GTK main context.
    pub async fn spawn(commands: async_channel::Sender<Command>) -> Result<Self, ksni::Error> {
        let handle = Model {
            state: State::default(),
            commands,
        }
        .spawn()
        .await?;

        // Updates go through one task rather than being spawned per change. `Handle::update`
        // awaits a lock on ksni's thread, so two of them in flight could take it in the
        // order they got there rather than the order the daemon spoke in, and the panel
        // would settle on a stale reading until the next poll.
        let (outbox, inbox) = async_channel::unbounded::<State>();
        glib::spawn_future_local(async move {
            while let Ok(state) = inbox.recv().await {
                if handle
                    .update(|model: &mut Model| model.state = state)
                    .await
                    .is_none()
                {
                    tracing::warn!("the tray service is gone; stopping updates");
                    return;
                }
            }
        });

        Ok(Self { outbox })
    }

    /// Tells the panel what the interface now knows. Never blocks.
    pub fn show(&self, statuses: &[ProviderStatus], titles: &model::Titles, connected: bool) {
        if self
            .outbox
            .try_send(State::of(statuses, titles, connected))
            .is_err()
        {
            tracing::debug!("the tray is no longer accepting updates");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{
        AccountId, ProviderDefinition, ProviderId, Snapshot, Timestamp, Window, WindowKey,
        WindowLength,
    };

    fn window(seconds: u64, used: f64) -> Window {
        Window {
            key: WindowKey::named(&format!("w{seconds}")),
            title: format!("{seconds}s"),
            subtitle: None,
            used_percent: used,
            resets_at: None,
            length: WindowLength::from_secs(seconds),
        }
    }

    fn reading(provider: &str, account: &str, windows: Vec<Window>) -> ProviderStatus {
        let mut status =
            ProviderStatus::pending(&ProviderId::new(provider), &AccountId::new(account));
        status.set_reading(&Snapshot {
            provider: ProviderId::new(provider),
            account: AccountId::new(account),
            captured_at: Timestamp::from_unix(1_785_700_000).expect("plausible"),
            windows,
            details: Vec::new(),
        });
        status
    }

    fn pending(provider: &str) -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new(provider), &AccountId::default())
    }

    #[test]
    fn a_row_reports_the_shortest_window_not_the_fullest_one() {
        let status = reading(
            "claude",
            "default",
            vec![window(604_800, 91.0), window(18_000, 12.0)],
        );
        let rows = entries(&[status], &model::Titles::new());
        assert_eq!(
            rows[0].value, "12%",
            "the five-hour window is the one the card leads with"
        );
    }

    #[test]
    fn the_menu_is_in_the_order_it_was_given_and_not_one_of_its_own() {
        let statuses = [
            reading("kimi", "default", vec![window(18_000, 10.0)]),
            reading("zai", "default", vec![window(18_000, 90.0)]),
        ];
        let rows = entries(&statuses, &model::Titles::new());
        assert_eq!(
            rows[0].line(),
            "Kimi — 10%",
            "the grid's order is the user's, and the panel does not have an opinion"
        );
        assert_eq!(rows[1].line(), "Z.ai — 90%");
    }

    #[test]
    fn an_account_with_no_reading_says_why_rather_than_showing_a_number() {
        let mut status = pending("codex");
        status.set_state(tidemark_types::ProviderState::NoCredential, None);
        let rows = entries(&[status], &model::Titles::new());
        assert_eq!(rows[0].line(), "Codex — no key");
    }

    #[test]
    fn an_account_that_kept_its_last_reading_still_shows_it() {
        let mut status = reading("zai", "default", vec![window(18_000, 44.0)]);
        status.set_state(tidemark_types::ProviderState::RateLimited, None);
        let rows = entries(&[status], &model::Titles::new());
        assert_eq!(
            rows[0].line(),
            "Z.ai — 44%",
            "a failed poll does not blank the numbers on the card either"
        );
    }

    #[test]
    fn two_accounts_of_one_provider_are_told_apart_and_one_is_not() {
        let statuses = [
            reading("zai", "work", vec![window(18_000, 90.0)]),
            reading("zai", "home", vec![window(18_000, 10.0)]),
            reading("kimi", "default", vec![window(18_000, 50.0)]),
        ];
        let rows = entries(&statuses, &model::Titles::new());
        let lines: Vec<String> = rows.iter().map(Entry::line).collect();
        assert_eq!(
            lines,
            ["Z.ai (work) — 90%", "Z.ai (home) — 10%", "Kimi — 50%"]
        );
    }

    #[test]
    fn nothing_configured_is_an_empty_list_rather_than_a_placeholder_row() {
        assert!(entries(&[], &model::Titles::new()).is_empty());
    }

    #[test]
    fn a_row_says_the_catalogs_spelling_of_the_providers_name() {
        // The panel and the settings dialog must not spell one provider two ways — the
        // catalog says "ClinePass", and capitalising the slug would say "Clinepass".
        let status = reading("clinepass", "default", vec![window(18_000, 50.0)]);
        let titles = model::titles(&[ProviderDefinition {
            provider: "clinepass".to_owned(),
            title: "ClinePass".to_owned(),
            credential: "key".to_owned(),
            credential_hint: "ClinePass console.".to_owned(),
            external: None,
            options: Vec::new(),
        }]);
        assert_eq!(
            entries(&[status], &titles)[0].line(),
            "ClinePass — 50%",
            "a slug this client has no title for keeps its capitalised spelling"
        );
    }

    #[test]
    fn attention_is_the_threshold_the_bar_and_the_notification_use() {
        let below = reading("zai", "default", vec![window(18_000, DANGER_AT - 0.1)]);
        let at = reading("zai", "default", vec![window(18_000, DANGER_AT)]);
        assert!(!needs_attention(&[below]));
        assert!(needs_attention(&[at]));
    }

    #[test]
    fn attention_looks_at_every_window_not_only_the_shortest() {
        // The weekly window is the one that is nearly gone; the dominant five-hour one is
        // empty. A panel that only watched the dominant window would say nothing.
        let status = reading(
            "claude",
            "default",
            vec![window(18_000, 3.0), window(604_800, 99.0)],
        );
        assert!(needs_attention(&[status]));
    }

    #[test]
    fn the_panel_receives_the_same_full_colour_icon_as_the_application() {
        let (commands, _inbox) = async_channel::unbounded();
        let tray = Model {
            state: State::default(),
            commands,
        };

        assert_eq!(ksni::Tray::icon_name(&tray), ids::APP_ID);
        assert_eq!(ksni::Tray::attention_icon_name(&tray), ids::APP_ID);
    }
}
