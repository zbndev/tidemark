//! Application preferences: behavior, startup, network, and local data.
//!
//! The daemon owns every value. A row the user moves shows its new state at once and
//! waits, disabled, for the answer; a refusal redraws everything from what the daemon last
//! said and says why.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;

use slint::{ComponentHandle, SharedString};
use tidemark_types::{DataInfo, Preferences};

use crate::alert::{Alerts, Question};
use crate::bus::DaemonProxy;
use crate::provider_settings::reason;
use crate::window::spawn;
use crate::{AppWindow, Prefs};

const RETENTION_VALUES: [&str; 3] = [
    Preferences::RETENTION_FOREVER,
    Preferences::RETENTION_SIX_MONTHS,
    Preferences::RETENTION_ONE_YEAR,
];

const STARTUP_VALUES: [&str; 3] = [
    Preferences::STARTUP_APP,
    Preferences::STARTUP_DAEMON,
    Preferences::STARTUP_OFF,
];

const THEME_VALUES: [&str; 3] = [
    Preferences::THEME_SYSTEM,
    Preferences::THEME_LIGHT,
    Preferences::THEME_DARK,
];

const PROXY_VALUES: [&str; 4] = [
    Preferences::PROXY_OFF,
    Preferences::PROXY_HTTP,
    Preferences::PROXY_HTTPS,
    Preferences::PROXY_SOCKS5,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitchKind {
    ReleaseCheck,
    MinimizeOnClose,
    RefreshAuto,
    ColumnsAuto,
}

impl SwitchKind {
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "release" => Some(Self::ReleaseCheck),
            "minimize" => Some(Self::MinimizeOnClose),
            "refresh-auto" => Some(Self::RefreshAuto),
            "columns-auto" => Some(Self::ColumnsAuto),
            _ => None,
        }
    }

    fn busy(self) -> Busy {
        match self {
            Self::ReleaseCheck => Busy::Release,
            Self::MinimizeOnClose => Busy::Minimize,
            Self::RefreshAuto => Busy::RefreshAuto,
            Self::ColumnsAuto => Busy::ColumnsAuto,
        }
    }
}

/// A row waiting for the daemon.
#[derive(Debug, Clone, Copy)]
enum Busy {
    Minimize,
    Startup,
    Theme,
    RefreshAuto,
    Minutes,
    ColumnsAuto,
    Columns,
    Proxy,
    Release,
    Retention,
    Clear,
}

/// Whether an incomplete proxy is the user's mistake or just the middle of typing one in.
#[derive(Debug, Clone, Copy)]
enum Complaint {
    /// A deliberate submit: mark the row that is wrong and say why.
    Loud,
    /// A mode was chosen and the rest is still to come: focus, do not scold.
    Silent,
}

#[derive(Debug, Clone, Copy)]
enum ProxyField {
    Host,
    Port,
}

type Daemon = Rc<dyn Fn() -> Option<DaemonProxy<'static>>>;

pub struct PreferencesDialog {
    ui: slint::Weak<AppWindow>,
    daemon: Daemon,
    alerts: Rc<Alerts>,
    preferences: RefCell<Preferences>,
    data: RefCell<DataInfo>,
    on_closed: Box<dyn Fn()>,
    closed: Cell<bool>,
}

impl std::fmt::Debug for PreferencesDialog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreferencesDialog")
            .field("preferences", &self.preferences)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl PreferencesDialog {
    pub fn open(
        ui: &AppWindow,
        daemon: Daemon,
        alerts: Rc<Alerts>,
        preferences: &Preferences,
        data: &DataInfo,
        on_closed: impl Fn() + 'static,
    ) -> Rc<Self> {
        let dialog = Rc::new(Self {
            ui: ui.as_weak(),
            daemon,
            alerts,
            preferences: RefCell::new(preferences.clone()),
            data: RefCell::new(data.clone()),
            on_closed: Box::new(on_closed),
            closed: Cell::new(false),
        });
        dialog.wire(ui);
        let prefs = ui.global::<Prefs>();
        prefs.set_page(0);
        dialog.render();
        prefs.set_open(true);
        dialog
    }

    fn wire(self: &Rc<Self>, ui: &AppWindow) {
        let prefs = ui.global::<Prefs>();
        let weak = Rc::downgrade(self);
        let with = |action: fn(&Rc<Self>)| {
            let weak = weak.clone();
            move || {
                if let Some(dialog) = weak.upgrade() {
                    action(&dialog);
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
        let counted = |action: fn(&Rc<Self>, u32)| {
            let weak = weak.clone();
            move |count: i32| {
                if let (Some(dialog), Ok(count)) = (weak.upgrade(), u32::try_from(count)) {
                    action(&dialog, count);
                }
            }
        };

        prefs.on_close(with(Self::close));
        prefs.on_set_switch({
            let weak = weak.clone();
            move |id: SharedString, enabled| {
                if let (Some(dialog), Some(kind)) = (weak.upgrade(), SwitchKind::from_id(&id)) {
                    dialog.change_switch(kind, enabled);
                }
            }
        });
        prefs.on_set_minutes(counted(Self::set_minutes));
        prefs.on_set_columns(counted(Self::set_columns));
        prefs.on_choose_startup(indexed(Self::choose_startup));
        prefs.on_choose_theme(indexed(Self::choose_theme));
        prefs.on_choose_retention(indexed(Self::choose_retention));
        prefs.on_choose_proxy_mode(indexed(|dialog, _| {
            dialog.submit_proxy(Complaint::Silent);
        }));
        prefs.on_apply_proxy(with(|dialog| dialog.submit_proxy(Complaint::Loud)));
        prefs.on_clear_history(with(Self::clear_history));
    }

    /// Takes what the daemon now says, and redraws every row from it.
    pub fn apply(&self, preferences: &Preferences, data: &DataInfo) {
        if self.closed.get() {
            return;
        }
        self.preferences.replace(preferences.clone());
        self.data.replace(data.clone());
        self.render();
    }

    fn render(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let prefs = ui.global::<Prefs>();
        let preferences = self.preferences.borrow();
        let data = self.data.borrow();

        prefs.set_minimize_on_close(preferences.minimize_on_close);
        let (index, unknown) = choice(&STARTUP_VALUES, &preferences.startup_mode);
        prefs.set_startup(index);
        prefs.set_startup_unknown(unknown.into());
        let theme = preferences
            .theme
            .as_deref()
            .unwrap_or(Preferences::THEME_SYSTEM);
        let (index, unknown) = choice(&THEME_VALUES, theme);
        prefs.set_theme(index);
        prefs.set_theme_unknown(unknown.into());
        prefs.set_refresh_auto(preferences.refresh_mode == Preferences::REFRESH_AUTO);
        prefs.set_refresh_minutes(clamp_to_i32(preferences.refresh_minutes));
        prefs.set_columns_auto(preferences.columns_auto.unwrap_or(true));
        prefs.set_max_columns(clamp_to_i32(preferences.max_columns.unwrap_or(3)));

        let (index, unknown) = choice(&PROXY_VALUES, &preferences.proxy_mode);
        prefs.set_proxy_mode(index);
        prefs.set_proxy_mode_unknown(unknown.into());
        let port = port_text(preferences.proxy_port);
        prefs.set_proxy_host(preferences.proxy_host.as_str().into());
        prefs.set_stored_host(preferences.proxy_host.as_str().into());
        prefs.set_proxy_port(port.as_str().into());
        prefs.set_stored_port(port.into());
        prefs.set_host_error(false);
        prefs.set_port_error(false);
        prefs.set_release_check(preferences.release_check && data.release_check_available);
        prefs.set_release_available(data.release_check_available);

        let (index, unknown) = choice(&RETENTION_VALUES, &preferences.history_retention);
        prefs.set_retention(index);
        prefs.set_retention_unknown(unknown.into());
        prefs.set_config_path(display_path(&data.config_path).into());
        prefs.set_history_path(display_path(&data.history_path).into());
        prefs.set_history_size(format_bytes(data.history_bytes).into());
        prefs.set_key_schema(data.key_schema.as_str().into());
        prefs.set_token_schema(data.token_schema.as_str().into());
    }

    fn close(self: &Rc<Self>) {
        if self.closed.replace(true) {
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<Prefs>().set_open(false);
        }
        (self.on_closed)();
    }

    fn toast(&self, message: &str) {
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<Prefs>().invoke_show_toast(message.into());
        }
    }

    fn set_busy(&self, busy: Busy, on: bool) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let prefs = ui.global::<Prefs>();
        match busy {
            Busy::Minimize => prefs.set_busy_minimize(on),
            Busy::Startup => prefs.set_busy_startup(on),
            Busy::Theme => prefs.set_busy_theme(on),
            Busy::RefreshAuto => prefs.set_busy_refresh_auto(on),
            Busy::Minutes => prefs.set_busy_minutes(on),
            Busy::ColumnsAuto => prefs.set_busy_columns_auto(on),
            Busy::Columns => prefs.set_busy_columns(on),
            Busy::Proxy => prefs.set_busy_proxy(on),
            Busy::Release => prefs.set_busy_release(on),
            Busy::Retention => prefs.set_busy_retention(on),
            Busy::Clear => prefs.set_busy_clear(on),
        }
    }

    /// Sends one change with its row disabled, which is what bounds how fast a held
    /// stepper or a restless switch can send. A refusal redraws from the daemon's state.
    fn send<F, Fut>(
        self: &Rc<Self>,
        busy: Busy,
        request: F,
        accepted: impl FnOnce(&mut Preferences) + 'static,
    ) where
        F: FnOnce(DaemonProxy<'static>) -> Fut + 'static,
        Fut: Future<Output = zbus::Result<()>> + 'static,
    {
        let Some(proxy) = (self.daemon)() else {
            self.toast("Tidemark is not running.");
            self.render();
            return;
        };
        self.set_busy(busy, true);
        let dialog = Rc::clone(self);
        spawn(async move {
            match request(proxy).await {
                Ok(()) => accepted(&mut dialog.preferences.borrow_mut()),
                Err(error) => {
                    dialog.render();
                    dialog.toast(&reason(&error));
                }
            }
            dialog.set_busy(busy, false);
        });
    }

    fn change_switch(self: &Rc<Self>, kind: SwitchKind, enabled: bool) {
        self.send(
            kind.busy(),
            move |proxy| async move {
                match kind {
                    SwitchKind::ReleaseCheck => proxy.set_release_check(enabled).await,
                    SwitchKind::MinimizeOnClose => proxy.set_minimize_on_close(enabled).await,
                    SwitchKind::RefreshAuto => {
                        proxy.set_refresh_mode(refresh_mode_for(enabled)).await
                    }
                    SwitchKind::ColumnsAuto => proxy.set_columns_auto(enabled).await,
                }
            },
            move |preferences| match kind {
                SwitchKind::ReleaseCheck => preferences.release_check = enabled,
                SwitchKind::MinimizeOnClose => preferences.minimize_on_close = enabled,
                SwitchKind::RefreshAuto => {
                    preferences.refresh_mode = refresh_mode_for(enabled).to_owned();
                }
                SwitchKind::ColumnsAuto => preferences.columns_auto = Some(enabled),
            },
        );
    }

    fn set_minutes(self: &Rc<Self>, minutes: u32) {
        self.send(
            Busy::Minutes,
            move |proxy| async move { proxy.set_refresh_minutes(minutes).await },
            move |preferences| preferences.refresh_minutes = minutes,
        );
        // The stepper shows the new value while it waits.
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<Prefs>()
                .set_refresh_minutes(clamp_to_i32(minutes));
        }
    }

    fn set_columns(self: &Rc<Self>, columns: u32) {
        self.send(
            Busy::Columns,
            move |proxy| async move { proxy.set_max_columns(columns).await },
            move |preferences| preferences.max_columns = Some(columns),
        );
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<Prefs>().set_max_columns(clamp_to_i32(columns));
        }
    }

    fn choose_startup(self: &Rc<Self>, index: usize) {
        let Some(mode) = STARTUP_VALUES.get(index) else {
            return;
        };
        self.send(
            Busy::Startup,
            move |proxy| async move { proxy.set_startup_mode(mode).await },
            move |preferences| preferences.startup_mode = (*mode).to_owned(),
        );
    }

    fn choose_theme(self: &Rc<Self>, index: usize) {
        let Some(theme) = THEME_VALUES.get(index) else {
            return;
        };
        self.send(
            Busy::Theme,
            move |proxy| async move { proxy.set_theme(theme).await },
            move |preferences| preferences.theme = Some((*theme).to_owned()),
        );
    }

    fn choose_retention(self: &Rc<Self>, index: usize) {
        let Some(retention) = RETENTION_VALUES.get(index) else {
            return;
        };
        self.send(
            Busy::Retention,
            move |proxy| async move { proxy.set_history_retention(retention).await },
            move |preferences| preferences.history_retention = (*retention).to_owned(),
        );
    }

    /// Sends the proxy the three rows currently describe, read from the rows rather than
    /// from what is stored.
    ///
    /// A mode with no host or no port yet is **not** sent. The daemon would refuse it and
    /// be right to, but choosing `SOCKS5` before typing where it is, is the normal way to
    /// fill this group in, and answering the first half of that with an error is answering
    /// the wrong thing: the row that still needs typing is focused, and only a deliberate
    /// submit of an incomplete one is marked as wrong.
    fn submit_proxy(self: &Rc<Self>, complaint: Complaint) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let prefs = ui.global::<Prefs>();
        let Some(mode) = usize::try_from(prefs.get_proxy_mode())
            .ok()
            .and_then(|index| PROXY_VALUES.get(index))
        else {
            return;
        };
        let host = prefs.get_proxy_host().trim().to_owned();
        prefs.set_host_error(false);
        prefs.set_port_error(false);
        let Some(port) = parse_port(&prefs.get_proxy_port()) else {
            prefs.set_port_error(true);
            self.focus(ProxyField::Port);
            if matches!(complaint, Complaint::Loud) {
                self.toast("A proxy port is a number from 1 to 65535");
            }
            return;
        };
        if *mode != Preferences::PROXY_OFF {
            let incomplete = if host.is_empty() {
                Some(ProxyField::Host)
            } else if port == 0 {
                Some(ProxyField::Port)
            } else {
                None
            };
            if let Some(field) = incomplete {
                if matches!(complaint, Complaint::Loud) {
                    match field {
                        ProxyField::Host => prefs.set_host_error(true),
                        ProxyField::Port => prefs.set_port_error(true),
                    }
                }
                self.focus(field);
                return;
            }
        }

        let Some(proxy) = (self.daemon)() else {
            self.toast("Tidemark is not running.");
            self.render();
            return;
        };
        self.set_busy(Busy::Proxy, true);
        let dialog = Rc::clone(self);
        spawn(async move {
            match proxy.set_proxy(mode, &host, port).await {
                Ok(()) => {
                    {
                        let mut preferences = dialog.preferences.borrow_mut();
                        preferences.proxy_mode = (*mode).to_owned();
                        preferences.proxy_host = host;
                        preferences.proxy_port = port;
                    }
                    dialog.toast("Proxy updated");
                }
                Err(error) => dialog.toast(&reason(&error)),
            }
            // Either way the rows are redrawn from the state that is now authoritative.
            dialog.set_busy(Busy::Proxy, false);
            dialog.render();
        });
    }

    fn focus(&self, field: ProxyField) {
        if let Some(ui) = self.ui.upgrade() {
            let prefs = ui.global::<Prefs>();
            prefs.set_focus_target(match field {
                ProxyField::Host => 0,
                ProxyField::Port => 1,
            });
            prefs.set_focus_serial(prefs.get_focus_serial() + 1);
        }
    }

    fn clear_history(self: &Rc<Self>) {
        self.set_busy(Busy::Clear, true);
        let dialog = Rc::clone(self);
        spawn(async move {
            let answer = dialog
                .alerts
                .ask(Question::destructive(
                    "Clear history?".to_owned(),
                    "This permanently deletes recorded quota history and notification records. \
                     Provider accounts and credentials are not affected.",
                    "Clear History",
                ))
                .await;
            if answer.confirmed() {
                match (dialog.daemon)() {
                    None => dialog.toast("Tidemark is not running."),
                    Some(proxy) => match proxy.clear_history().await {
                        Ok(()) => {
                            dialog.toast("History cleared");
                            if let Ok(data) = proxy.get_data_info().await {
                                let preferences = dialog.preferences.borrow().clone();
                                dialog.apply(&preferences, &data);
                            }
                        }
                        Err(error) => dialog.toast(&reason(&error)),
                    },
                }
            }
            dialog.set_busy(Busy::Clear, false);
        });
    }
}

/// A named daemon value as a row's index, or -1 and what to say instead.
///
/// A value this build does not know is kept visible and untouchable rather than guessed:
/// the row shows no choice, is disabled so it cannot be changed by accident, and says what
/// the daemon actually reported.
fn choice(values: &[&str], raw: &str) -> (i32, String) {
    match values.iter().position(|value| *value == raw) {
        Some(index) => (i32::try_from(index).unwrap_or(-1), String::new()),
        None => (
            -1,
            format!("Unsupported value {raw:?} reported by the daemon."),
        ),
    }
}

/// Zero is "unset" on the wire and has to read as empty: a port row showing `0` invites
/// the user to leave it, and `0` is not a port.
fn port_text(port: u16) -> String {
    match port {
        0 => String::new(),
        port => port.to_string(),
    }
}

/// An empty row is "no port yet"; anything else must be a real one.
fn parse_port(typed: &str) -> Option<u16> {
    let typed = typed.trim();
    if typed.is_empty() {
        Some(0)
    } else {
        typed.parse::<u16>().ok().filter(|port| *port != 0)
    }
}

/// The named mode a switch state commits.
fn refresh_mode_for(auto_active: bool) -> &'static str {
    if auto_active {
        Preferences::REFRESH_AUTO
    } else {
        Preferences::REFRESH_MANUAL
    }
}

fn clamp_to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn display_path(path: &str) -> String {
    if path.is_empty() {
        "Unavailable until the daemon is restarted".into()
    } else {
        path.into()
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a size shown to one decimal place"
)]
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    match bytes {
        0..KIB => format!("{bytes} bytes"),
        KIB..MIB => format!("{:.1} KiB", bytes as f64 / KIB as f64),
        MIB..GIB => format!("{:.1} MiB", bytes as f64 / MIB as f64),
        _ => format!("{:.1} GiB", bytes as f64 / GIB as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_value_selects_its_row() {
        assert_eq!(
            choice(&RETENTION_VALUES, Preferences::RETENTION_FOREVER).0,
            0
        );
        assert_eq!(
            choice(&RETENTION_VALUES, Preferences::RETENTION_ONE_YEAR).0,
            2
        );
        assert_eq!(choice(&STARTUP_VALUES, Preferences::STARTUP_DAEMON).0, 1);
        assert_eq!(choice(&THEME_VALUES, Preferences::THEME_DARK).0, 2);
        assert_eq!(choice(&PROXY_VALUES, Preferences::PROXY_SOCKS5).0, 3);
    }

    #[test]
    fn an_unknown_value_is_shown_rather_than_guessed() {
        let (index, said) = choice(&PROXY_VALUES, "socks4");
        assert_eq!(index, -1);
        assert!(said.contains("\"socks4\""), "{said}");
        assert!(choice(&THEME_VALUES, "light").1.is_empty());
    }

    #[test]
    fn switch_ids_name_the_four_switches() {
        assert_eq!(
            SwitchKind::from_id("release"),
            Some(SwitchKind::ReleaseCheck)
        );
        assert_eq!(
            SwitchKind::from_id("minimize"),
            Some(SwitchKind::MinimizeOnClose)
        );
        assert_eq!(
            SwitchKind::from_id("refresh-auto"),
            Some(SwitchKind::RefreshAuto)
        );
        assert_eq!(
            SwitchKind::from_id("columns-auto"),
            Some(SwitchKind::ColumnsAuto)
        );
        assert_eq!(SwitchKind::from_id("theme"), None);
    }

    #[test]
    fn a_switch_state_maps_back_to_one_of_the_named_refresh_modes() {
        assert_eq!(refresh_mode_for(true), Preferences::REFRESH_AUTO);
        assert_eq!(refresh_mode_for(false), Preferences::REFRESH_MANUAL);
    }

    #[test]
    fn an_unset_port_reads_as_empty_and_empty_reads_as_unset() {
        assert_eq!(port_text(0), "");
        assert_eq!(port_text(1080), "1080");
        assert_eq!(parse_port(" "), Some(0));
        assert_eq!(parse_port("1080"), Some(1080));
        assert_eq!(parse_port("0"), None);
        assert_eq!(parse_port("70000"), None);
        assert_eq!(parse_port("proxy"), None);
    }

    #[test]
    fn database_sizes_are_readable_without_losing_small_values() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(512), "512 bytes");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn a_path_the_daemon_could_not_name_says_so() {
        assert_eq!(
            display_path(""),
            "Unavailable until the daemon is restarted"
        );
        assert_eq!(display_path("/x/config.toml"), "/x/config.toml");
    }
}
