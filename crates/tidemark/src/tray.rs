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
use tidemark_types::{DANGER_AT, ProviderStatus, present};

use crate::format;
use crate::model;

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
/// apart — preferring the account's label, falling back to its id. One account per provider
/// is the ordinary case and `Claude (default)` would be a word of noise on every line of the
/// menu.
fn label(all: &[ProviderStatus], status: &ProviderStatus, titles: &model::Titles) -> String {
    let name = model::name(titles, &status.provider);
    let shared = all
        .iter()
        .filter(|other| other.provider == status.provider)
        .count()
        > 1;
    if shared {
        let account = status.account_label.as_deref().unwrap_or(&status.account);
        format!("{name} ({account})")
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

    /// Hands a command to the interface. Never blocks and never panics: the channel is
    /// unbounded, and a closed one means the window is already going away.
    fn send(&self, command: Command) {
        if self.commands.try_send(command).is_err() {
            tracing::debug!(?command, "the interface is no longer listening to the tray");
        }
    }
}

/// The platform-neutral representation of the panel icon's menu.
#[derive(Debug)]
enum MenuItem {
    Caption(String),
    Action {
        label: String,
        // Only the ksni backend resolves these freedesktop icon names; muda on Windows
        // would not, so it never reads the field. Inert on Linux.
        #[cfg_attr(windows, allow(dead_code))]
        icon: String,
        enabled: bool,
        command: Command,
    },
    Separator,
}

/// What a native tray backend needs from the shared model.
///
/// Todo 15 implements this contract with `tray-icon` on Windows. Keeping it free of native
/// tray types lets the channel bridge and state updates remain common to both backends.
trait Backend {
    fn set_icon(&self) -> bool;
    fn set_tooltip(&self) -> String;
    fn set_menu(&self) -> Vec<MenuItem>;
    // Only the ksni backend has a status-notifier watcher to go offline; tray-icon has
    // none, so the Windows build has no caller. The surface stays shared (todo 9).
    #[cfg_attr(windows, allow(dead_code))]
    fn handle_watcher_offline(&self) -> bool;
}

impl Backend for Model {
    fn set_icon(&self) -> bool {
        self.state.attention
    }

    fn set_tooltip(&self) -> String {
        match self.placeholder() {
            Some(reason) => reason.to_owned(),
            None => self.state.entries[0].line(),
        }
    }

    fn set_menu(&self) -> Vec<MenuItem> {
        let mut items = match self.placeholder() {
            Some(reason) => vec![MenuItem::Caption(reason.to_owned())],
            None => self
                .state
                .entries
                .iter()
                // An account row shows the window rather than being dead text: the panel
                // is where the user noticed the number, and the window is where they can
                // do anything about it.
                .map(|entry| MenuItem::Action {
                    label: entry.line(),
                    icon: String::new(),
                    enabled: true,
                    command: Command::Present,
                })
                .collect(),
        };

        items.push(MenuItem::Separator);
        items.push(MenuItem::Action {
            label: "Open Tidemark".to_owned(),
            icon: "window-new-symbolic".to_owned(),
            enabled: true,
            command: Command::Present,
        });
        items.push(MenuItem::Action {
            label: "Refresh now".to_owned(),
            icon: "view-refresh-symbolic".to_owned(),
            enabled: self.state.connected,
            command: Command::Refresh,
        });
        items.push(MenuItem::Separator);
        items.push(MenuItem::Action {
            label: "Quit".to_owned(),
            icon: "application-exit-symbolic".to_owned(),
            enabled: true,
            command: Command::Quit,
        });
        items
    }

    fn handle_watcher_offline(&self) -> bool {
        true
    }
}

#[cfg(unix)]
mod backend {
    use super::*;
    use ksni::TrayMethods;
    use tidemark_types::ids;

    /// The icon the panel shows. It deliberately uses the same full-colour icon name as the
    /// application: `data/icons` supplies native small sizes so a panel never has to enlarge a
    /// tiny fallback pixmap, and the `PKGBUILD` installs them all.
    const ICON: &str = ids::APP_ID;

    pub type Error = ksni::Error;

    pub struct Handle(ksni::Handle<Model>);

    pub async fn spawn(model: Model) -> Result<Handle, Error> {
        Ok(Handle(model.spawn().await?))
    }

    impl Handle {
        pub async fn update(&self, state: State) -> bool {
            self.0
                .update(|model: &mut Model| model.state = state)
                .await
                .is_some()
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
            if self.set_icon() {
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
            ksni::ToolTip {
                icon_name: ICON.to_owned(),
                title: "Tidemark".to_owned(),
                description: self.set_tooltip(),
                ..Default::default()
            }
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            self.set_menu().into_iter().map(menu_item).collect()
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
            self.handle_watcher_offline()
        }
    }

    fn menu_item(item: MenuItem) -> ksni::MenuItem<Model> {
        match item {
            MenuItem::Caption(label) => ksni::menu::StandardItem {
                label: mnemonics(&label),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Action {
                label,
                icon,
                enabled,
                command,
            } => ksni::menu::StandardItem {
                label: mnemonics(&label),
                icon_name: icon,
                enabled,
                activate: Box::new(move |model: &mut Model| model.send(command)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator => ksni::MenuItem::Separator,
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

    #[cfg(test)]
    mod tests {
        use super::*;

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
}

#[cfg(windows)]
mod backend {
    //! The Windows tray: `tray-icon` on a thread of its own, speaking to the GTK main
    //! context over the same [`Command`] channel the ksni backend uses. tray-icon is not
    //! GTK-integrated and Windows delivers its menu and click events only through a win32
    //! message loop running on the thread that created the icon, so this thread owns the
    //! icon, the menu and the pump, and nothing here ever touches a widget: a click puts
    //! a [`Command`] on the channel and [`Tray::spawn`]'s task on the main context acts
    //! on it, exactly as on Linux.
    //
    // The win32 message pump (GetMessageW/DispatchMessageW/PostThreadMessageW) that
    // tray-icon's Windows backend requires is unsafe FFI; the workspace-wide deny is
    // lifted for this module only, as documented for the plan's §15 dependency list.
    #![allow(unsafe_code)]

    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender, channel};
    // The shared menu model's own `MenuItem` keeps its name; muda's is spelled out at
    // its uses so the two never blur.
    use tray_icon::menu::{Menu, MenuEvent, MenuId, PredefinedMenuItem};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, TranslateMessage, WM_QUIT,
    };

    #[path = "../../tray_icon_rgba.rs"]
    mod icon_rgba;

    /// A thread message no window here uses, posted only to wake the pump so an update
    /// sitting in the channel is drained without waiting for real input.
    const WAKE: u32 = 0x8000; // WM_APP

    /// What went wrong putting the icon up.
    #[derive(Debug)]
    pub struct Error(String);

    impl std::fmt::Display for Error {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.0)
        }
    }

    impl std::error::Error for Error {}

    /// What the thread needs to hear about, from the interface's side.
    enum Message {
        Update(State),
        Shutdown,
    }

    /// The icon and menu, from outside the tray thread.
    ///
    /// Dropping it takes the icon down: the shutdown message wakes the pump, the thread
    /// drops the `TrayIcon` and joins, and the window can close knowing nothing of its
    /// is left behind.
    #[derive(Debug)]
    pub struct Handle {
        thread_id: u32,
        outbox: Sender<Message>,
        join: Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            let _ = self.outbox.send(Message::Shutdown);
            stop_pump(self.thread_id);
            if let Some(join) = self.join.lock().expect("poisoned").take() {
                let _ = join.join();
            }
        }
    }

    pub async fn spawn(model: Model) -> Result<Handle, Error> {
        let (outbox, inbox) = channel::<Message>();
        let (thread_id_tx, thread_id_rx) = channel();
        let (ready_tx, ready_rx) = channel::<Result<(), Error>>();

        let join = std::thread::Builder::new()
            .name("tray-icon".to_owned())
            .spawn(move || {
                // SAFETY: GetCurrentThreadId is a plain id read.
                let thread_id = unsafe { GetCurrentThreadId() };
                if thread_id_tx.send(thread_id).is_err() {
                    return; // the spawner is gone; nothing here is worth putting up.
                }
                run(model, inbox, ready_tx).ok();
            })
            .map_err(|error| Error(error.to_string()))?;

        let thread_id = thread_id_rx
            .recv()
            .map_err(|_| Error("the tray thread exited before it started".to_owned()))?;
        ready_rx
            .recv()
            .map_err(|_| Error("the tray thread died while building the tray".to_owned()))??;

        Ok(Handle {
            thread_id,
            outbox,
            join: Mutex::new(Some(join)),
        })
    }

    impl Handle {
        pub async fn update(&self, state: State) -> bool {
            if self.outbox.send(Message::Update(state)).is_err() {
                return false;
            }
            wake_pump(self.thread_id);
            true
        }
    }

    /// Builds the tray, then runs the win32 message pump until told to stop.
    fn run(
        mut model: Model,
        inbox: Receiver<Message>,
        ready: Sender<Result<(), Error>>,
    ) -> Result<(), Error> {
        let tooltip = model.set_tooltip();
        let (menu, mut ids) = build_menu(&model)?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon())
            .with_tooltip(tooltip)
            // A left click shows the window, as the ksni backend's activate does; the
            // menu is the right button.
            .with_menu_on_left_click(false)
            .build()
            .map_err(|error| Error(error.to_string()))?;

        // Only now is the icon actually up, and the spawner may tell the interface.
        let _ = ready.send(Ok(()));

        let tray = {
            let mut owned = Some(tray);
            loop {
                // Events arrive on muda's own channel and are drained here, on the thread
                // that owns the menu, so `Model::send` stays the one way to the interface.
                while let Ok(event) = MenuEvent::receiver().try_recv() {
                    if let Some(command) = ids.get(&event.id.0) {
                        model.send(*command);
                    }
                }
                // Windows reports both halves of a click; the release is the one that
                // shows the window, so activate fires once per click as on Linux.
                while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        model.send(Command::Present);
                    }
                }
                while let Ok(message) = inbox.try_recv() {
                    match message {
                        Message::Update(state) => {
                            model.state = state;
                            apply(owned.as_ref(), &model, &mut ids);
                        }
                        Message::Shutdown => owned = None,
                    }
                }
                if owned.is_none() {
                    break; // dropping the TrayIcon removes the icon.
                }

                let mut message = MSG::default();
                // SAFETY: the message pump of the thread that owns the tray window, which
                // is what tray-icon's docs require on Windows. <= 0 is WM_QUIT or error.
                let received = unsafe { GetMessageW(&mut message, Some(HWND::default()), 0, 0) };
                if received.0 <= 0 {
                    break;
                }
                // SAFETY: the pumped message, translated and dispatched as win32 requires.
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            owned
        };

        drop(tray);
        Ok(())
    }

    /// Re-applies everything the new state changes, on the tray thread.
    ///
    /// The icon pixels do not differ between ordinary and attention, matching the ksni
    /// backend where `attention_icon_name` is the same icon; the flag is still read so a
    /// future attention icon has one place to land.
    fn apply(tray: Option<&TrayIcon>, model: &Model, ids: &mut HashMap<String, Command>) {
        let Some(tray) = tray else { return };
        let _attention = model.set_icon();
        let (menu, new_ids) = build_menu(model).expect("the menu was built once already");
        *ids = new_ids;
        let _ = tray.set_tooltip(Some(model.set_tooltip()));
        let _ = tray.set_icon(Some(icon()));
        tray.set_menu(Some(Box::new(menu)));
    }

    /// The shared model's menu as muda items, plus the id → command table the drained
    /// events read. Pure, so the mapping is testable without a tray.
    fn build_menu(model: &Model) -> Result<(Menu, HashMap<String, Command>), Error> {
        let menu = Menu::new();
        let mut ids = HashMap::new();
        for (index, item) in model.set_menu().into_iter().enumerate() {
            match item {
                MenuItem::Caption(label) => menu
                    .append(&tray_icon::menu::MenuItem::new(escape(&label), false, None))
                    .map_err(|error| Error(error.to_string()))?,
                MenuItem::Action {
                    label,
                    enabled,
                    command,
                    ..
                } => {
                    let id = index.to_string();
                    menu.append(&tray_icon::menu::MenuItem::with_id(
                        MenuId(id.clone()),
                        escape(&label),
                        enabled,
                        None,
                    ))
                    .map_err(|error| Error(error.to_string()))?;
                    ids.insert(id, command);
                }
                MenuItem::Separator => menu
                    .append(&PredefinedMenuItem::separator())
                    .map_err(|error| Error(error.to_string()))?,
            }
        }
        Ok((menu, ids))
    }

    /// The application icon, embedded rather than read from a theme: Windows tray icons
    /// take raw pixels, and no icon theme is guaranteed to be installed.
    fn icon() -> Icon {
        Icon::from_rgba(icon_rgba::ICON_RGBA.to_vec(), 32, 32).expect("the embedded icon is 32x32")
    }

    /// Escapes a label for win32 menus, which read a single `&` as the marker before an
    /// access key, the way the ksni backend escapes `_`.
    fn escape(label: &str) -> String {
        label.replace('&', "&&")
    }

    /// Wakes the pump so a queued update is seen without waiting for real input.
    fn wake_pump(thread_id: u32) {
        // SAFETY: posting a thread message to our own tray thread. It may already be
        // gone, which is fine and ignored.
        unsafe {
            let _ = PostThreadMessageW(thread_id, WAKE, WPARAM(0), LPARAM(0));
        }
    }

    /// Asks the pump to end via WM_QUIT, the one message that survives its blocking wait.
    fn stop_pump(thread_id: u32) {
        // SAFETY: as wake_pump; ignored when the thread is already gone.
        unsafe {
            let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn model(state: State) -> Model {
            let (commands, _inbox) = async_channel::unbounded();
            Model { state, commands }
        }

        fn labels(model: &Model) -> Vec<String> {
            model
                .set_menu()
                .into_iter()
                .map(|item| match item {
                    super::MenuItem::Caption(label) => format!("caption: {label}"),
                    super::MenuItem::Action { label, .. } => format!("action: {label}"),
                    super::MenuItem::Separator => "separator".to_owned(),
                })
                .collect()
        }

        #[test]
        fn the_windows_menu_is_the_shared_model_unchanged() {
            let state = State {
                entries: vec![Entry {
                    label: "Claude".to_owned(),
                    value: "12%".to_owned(),
                }],
                attention: false,
                connected: true,
            };
            assert_eq!(
                labels(&model(state)),
                [
                    "action: Claude — 12%",
                    "separator",
                    "action: Open Tidemark",
                    "action: Refresh now",
                    "separator",
                    "action: Quit",
                ]
            );
        }

        #[test]
        fn a_disconnected_daemon_disables_refresh_and_replaces_the_accounts() {
            let state = State {
                entries: vec![Entry {
                    label: "Claude".to_owned(),
                    value: "12%".to_owned(),
                }],
                attention: false,
                connected: false,
            };
            assert_eq!(
                labels(&model(state)),
                [
                    "caption: Waiting for Tidemark…",
                    "separator",
                    "action: Open Tidemark",
                    "action: Refresh now",
                    "separator",
                    "action: Quit",
                ]
            );
        }

        #[test]
        fn every_action_gets_an_id_that_marshals_to_its_command() {
            let tray = model(State::default());
            let (_, ids) = build_menu(&tray).expect("the menu builds");
            let actions: Vec<Option<Command>> = tray
                .set_menu()
                .into_iter()
                .enumerate()
                .map(|(index, item)| match item {
                    super::MenuItem::Action { command, .. } => {
                        assert!(
                            ids.contains_key(&index.to_string()),
                            "every action has an id"
                        );
                        Some(command)
                    }
                    _ => {
                        assert!(
                            !ids.contains_key(&index.to_string()),
                            "only actions have ids"
                        );
                        None
                    }
                })
                .collect();
            assert_eq!(
                actions.iter().flatten().count(),
                ids.len(),
                "one id per action, none shared"
            );
            for (index, command) in actions
                .into_iter()
                .enumerate()
                .filter_map(|(index, command)| command.map(|command| (index, command)))
            {
                assert_eq!(ids[&index.to_string()], command);
            }
        }

        #[test]
        fn a_menu_event_marshals_to_the_command_its_item_was_built_with() {
            let tray = model(State::default());
            let (_, ids) = build_menu(&tray).expect("the menu builds");
            let quit = ids
                .iter()
                .find(|(_, command)| **command == Command::Quit)
                .expect("the menu has a quit");
            let event = tray_icon::menu::MenuEvent {
                id: MenuId(quit.0.clone()),
            };
            assert_eq!(
                ids.get(&event.id.0),
                Some(&Command::Quit),
                "what the handler looks up"
            );
        }

        #[test]
        fn the_embedded_icon_is_whole() {
            assert_eq!(icon_rgba::ICON_RGBA.len(), 32 * 32 * 4);
        }
    }
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
    pub async fn spawn(commands: async_channel::Sender<Command>) -> Result<Self, backend::Error> {
        let handle = backend::spawn(Model {
            state: State::default(),
            commands,
        })
        .await?;

        // Updates go through one task rather than being spawned per change. `Handle::update`
        // awaits a lock on ksni's thread, so two of them in flight could take it in the
        // order they got there rather than the order the daemon spoke in, and the panel
        // would settle on a stale reading until the next poll.
        let (outbox, inbox) = async_channel::unbounded::<State>();
        glib::spawn_future_local(async move {
            while let Ok(state) = inbox.recv().await {
                if !handle.update(state).await {
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
    fn a_shared_provider_says_which_account_each_row_is() {
        let statuses = [
            reading("claude", "default", vec![window(18_000, 10.0)]),
            reading("claude", "work", vec![window(18_000, 20.0)]),
        ];
        let rows = entries(&statuses, &model::Titles::new());
        assert_eq!(rows[0].line(), "Claude (default) — 10%");
        assert_eq!(rows[1].line(), "Claude (work) — 20%");
    }

    #[test]
    fn a_shared_provider_prefers_the_account_label_to_its_id() {
        let mut second = reading("claude", "work", vec![window(18_000, 20.0)]);
        second.account_label = Some("Work Laptop".to_string());
        let statuses = [
            reading("claude", "default", vec![window(18_000, 10.0)]),
            second,
        ];
        let rows = entries(&statuses, &model::Titles::new());
        assert_eq!(rows[1].line(), "Claude (Work Laptop) — 20%");
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
            browser_auth: None,
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
}
