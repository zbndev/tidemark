//! Application preferences: behavior, startup, network, and local data.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;
use tidemark_types::{DataInfo, Preferences};

use crate::bus::DaemonProxy;

const RETENTION_VALUES: [&str; 3] = [
    Preferences::RETENTION_FOREVER,
    Preferences::RETENTION_SIX_MONTHS,
    Preferences::RETENTION_ONE_YEAR,
];

/// What the retention values above are called on screen, in the same order.
const RETENTION_LABELS: [&str; 3] = ["Forever", "6 months", "1 year"];

const STARTUP_VALUES: [&str; 3] = [
    Preferences::STARTUP_APP,
    Preferences::STARTUP_DAEMON,
    Preferences::STARTUP_OFF,
];

/// What the startup values above are called on screen, in the same order.
const STARTUP_LABELS: [&str; 3] = ["App and tray", "Daemon only", "Off"];

const PROXY_VALUES: [&str; 4] = [
    Preferences::PROXY_OFF,
    Preferences::PROXY_HTTP,
    Preferences::PROXY_HTTPS,
    Preferences::PROXY_SOCKS5,
];

/// What the proxy modes above are called on screen, in the same order.
const PROXY_LABELS: [&str; 4] = ["Off", "HTTP", "HTTPS", "SOCKS5"];

#[derive(Debug, Clone, Copy)]
enum SwitchKind {
    ReleaseCheck,
    MinimizeOnClose,
    RefreshAuto,
}

/// Whether an incomplete proxy is the user's mistake or just the middle of typing one in.
#[derive(Debug, Clone, Copy)]
enum Complaint {
    /// A deliberate submit: mark the row that is wrong and say why.
    Loud,
    /// A mode was chosen and the rest is still to come: focus, do not scold.
    Silent,
}

/// The standard libadwaita Preferences dialog and its authoritative daemon-backed state.
#[derive(Debug)]
pub struct PreferencesDialog {
    dialog: adw::PreferencesDialog,
    proxy: DaemonProxy<'static>,
    release_check: adw::SwitchRow,
    minimize_on_close: adw::SwitchRow,
    refresh_auto: adw::SwitchRow,
    refresh_minutes: adw::SpinRow,
    startup: adw::ComboRow,
    retention: adw::ComboRow,
    proxy_mode: adw::ComboRow,
    proxy_host: adw::EntryRow,
    proxy_port: adw::EntryRow,
    config_path: adw::ActionRow,
    history_path: adw::ActionRow,
    history_size: adw::ActionRow,
    key_schema: adw::ActionRow,
    token_schema: adw::ActionRow,
    preferences: RefCell<Preferences>,
    data: RefCell<DataInfo>,
    suppress: Cell<bool>,
}

impl PreferencesDialog {
    pub fn present(
        parent: &impl IsA<gtk::Widget>,
        proxy: DaemonProxy<'static>,
        preferences: Preferences,
        data: DataInfo,
        on_closed: impl Fn() + 'static,
    ) -> Rc<Self> {
        let dialog = adw::PreferencesDialog::builder()
            .title("Preferences")
            .content_width(620)
            .content_height(680)
            .build();

        let minimize_on_close = adw::SwitchRow::builder()
            .title("Minimize to tray on close")
            .subtitle("Keep Tidemark running when a tray icon can bring the window back.")
            .build();
        let startup_mode = adw::ComboRow::builder()
            .title("Start at login")
            .subtitle("Choose what starts with your graphical session.")
            .model(&gtk::StringList::new(&STARTUP_LABELS))
            .expression(gtk::PropertyExpression::new(
                gtk::StringObject::static_type(),
                None::<gtk::Expression>,
                "string",
            ))
            .use_subtitle(true)
            .build();

        let behavior = adw::PreferencesGroup::builder().title("Behavior").build();
        behavior.add(&minimize_on_close);
        let startup = adw::PreferencesGroup::builder().title("Startup").build();
        startup.add(&startup_mode);
        // The subtitle stays vague on purpose: which zone buys which pace is the daemon's
        // business, and a number here would be a second truth to keep in step with
        // `CONTEXT.md`.
        let refresh_auto = adw::SwitchRow::builder()
            .title("Auto")
            .subtitle("Refresh frequency adapts to how much quota is left.")
            .build();
        let refresh_minutes = adw::SpinRow::new(
            Some(&gtk::Adjustment::new(5.0, 1.0, 120.0, 1.0, 10.0, 0.0)),
            1.0,
            0,
        );
        refresh_minutes.set_title("Manual refresh frequency");
        refresh_minutes.set_subtitle("Minutes between polls when Auto is off.");
        let refresh_group = adw::PreferencesGroup::builder()
            .title("Providers refresh")
            .build();
        refresh_group.add(&refresh_auto);
        refresh_group.add(&refresh_minutes);
        let general = adw::PreferencesPage::builder()
            .title("General")
            .icon_name("preferences-system-symbolic")
            .build();
        general.add(&behavior);
        general.add(&startup);
        general.add(&refresh_group);
        dialog.add(&general);

        let proxy_mode = adw::ComboRow::builder()
            .title("Proxy")
            .subtitle("Route every request and every helper process through a proxy.")
            .model(&gtk::StringList::new(&PROXY_LABELS))
            .expression(gtk::PropertyExpression::new(
                gtk::StringObject::static_type(),
                None::<gtk::Expression>,
                "string",
            ))
            .use_subtitle(true)
            .build();
        // An apply button rather than a request per keystroke: `example.com` on the way to
        // `proxy.example.com` is eleven proxies nobody asked for, and each one would drop
        // every client the daemon holds.
        let proxy_host = adw::EntryRow::builder()
            .title("Host")
            .show_apply_button(true)
            .input_purpose(gtk::InputPurpose::Url)
            .build();
        let proxy_port = adw::EntryRow::builder()
            .title("Port")
            .show_apply_button(true)
            .input_purpose(gtk::InputPurpose::Digits)
            .build();
        let proxy_group = adw::PreferencesGroup::builder()
            .title("Proxy")
            .description(
                "Applies immediately, without restarting the background service. \
                 Requests to this machine never go through it.",
            )
            .build();
        proxy_group.add(&proxy_mode);
        proxy_group.add(&proxy_host);
        proxy_group.add(&proxy_port);

        let release_check = adw::SwitchRow::builder()
            .title("Check for updates")
            .subtitle("Ask GitHub for the latest Tidemark release once an hour.")
            .build();
        let release_group = adw::PreferencesGroup::builder()
            .title("Release Updates")
            .build();
        release_group.add(&release_check);

        // The release check lives here rather than on a page of its own: it is one switch,
        // and what it is a switch over is the network.
        let network = adw::PreferencesPage::builder()
            .title("Network")
            .icon_name("network-workgroup-symbolic")
            .build();
        network.add(&proxy_group);
        network.add(&release_group);
        dialog.add(&network);

        let retention = adw::ComboRow::builder()
            .title("Keep history")
            .subtitle("Older readings are deleted after this period.")
            .model(&gtk::StringList::new(&RETENTION_LABELS))
            .expression(gtk::PropertyExpression::new(
                gtk::StringObject::static_type(),
                None::<gtk::Expression>,
                "string",
            ))
            .use_subtitle(true)
            .build();
        let clear = gtk::Button::builder()
            .label("Clear History")
            .valign(gtk::Align::Center)
            .css_classes(["destructive-action"])
            .build();
        let clear_row = adw::ActionRow::builder()
            .title("Recorded quota history")
            .subtitle("Delete readings and notification records. Accounts and credentials stay.")
            .build();
        clear_row.add_suffix(&clear);

        let history_group = adw::PreferencesGroup::builder().title("History").build();
        history_group.add(&retention);
        history_group.add(&clear_row);

        let config_path = path_row("Configuration file");
        let history_path = path_row("History database");
        let history_size = adw::ActionRow::builder().title("Database size").build();
        let files = adw::PreferencesGroup::builder().title("Files").build();
        files.add(&config_path);
        files.add(&history_path);
        files.add(&history_size);

        let key_schema = path_row("API keys");
        let token_schema = path_row("OAuth sessions");
        let keyring = adw::PreferencesGroup::builder()
            .title("System Keyring")
            .description("Secrets stay in the desktop Secret Service, not in config.toml.")
            .build();
        keyring.add(&key_schema);
        keyring.add(&token_schema);

        let data_page = adw::PreferencesPage::builder()
            .title("Data")
            .icon_name("folder-documents-symbolic")
            .build();
        data_page.add(&history_group);
        data_page.add(&files);
        data_page.add(&keyring);
        dialog.add(&data_page);

        let settings = Rc::new(Self {
            dialog: dialog.clone(),
            proxy,
            release_check,
            minimize_on_close,
            refresh_auto,
            refresh_minutes,
            startup: startup_mode,
            retention,
            proxy_mode,
            proxy_host,
            proxy_port,
            config_path,
            history_path,
            history_size,
            key_schema,
            token_schema,
            preferences: RefCell::new(preferences.clone()),
            data: RefCell::new(data.clone()),
            suppress: Cell::new(false),
        });

        settings.connect_switch(&settings.release_check, SwitchKind::ReleaseCheck);
        settings.connect_switch(&settings.minimize_on_close, SwitchKind::MinimizeOnClose);
        settings.connect_switch(&settings.refresh_auto, SwitchKind::RefreshAuto);
        settings.connect_startup();
        settings.connect_retention();
        settings.connect_proxy();
        settings.connect_refresh_minutes();
        settings.connect_clear(&clear);
        settings.apply(&preferences, &data);

        dialog.connect_closed({
            let weak = Rc::downgrade(&settings);
            move |_| {
                if weak.upgrade().is_some() {
                    on_closed();
                }
            }
        });
        dialog.present(Some(parent));
        settings
    }

    pub fn apply(&self, preferences: &Preferences, data: &DataInfo) {
        *self.preferences.borrow_mut() = preferences.clone();
        *self.data.borrow_mut() = data.clone();
        self.suppress.set(true);
        self.release_check
            .set_active(preferences.release_check && data.release_check_available);
        self.minimize_on_close
            .set_active(preferences.minimize_on_close);
        self.refresh_auto
            .set_active(preferences.refresh_mode == Preferences::REFRESH_AUTO);
        self.refresh_minutes
            .set_value(f64::from(preferences.refresh_minutes));
        apply_named_choice(
            &self.startup,
            &STARTUP_LABELS,
            startup_index(&preferences.startup_mode),
            &preferences.startup_mode,
        );
        apply_named_choice(
            &self.retention,
            &RETENTION_LABELS,
            retention_index(&preferences.history_retention),
            &preferences.history_retention,
        );
        apply_named_choice(
            &self.proxy_mode,
            &PROXY_LABELS,
            proxy_index(&preferences.proxy_mode),
            &preferences.proxy_mode,
        );
        self.proxy_host.set_text(&preferences.proxy_host);
        // Zero is "unset" on the wire and has to read as empty here: a port row showing
        // `0` invites the user to leave it, and `0` is not a port.
        self.proxy_port.set_text(&match preferences.proxy_port {
            0 => String::new(),
            port => port.to_string(),
        });
        self.suppress.set(false);

        self.release_check
            .set_sensitive(data.release_check_available);
        self.release_check
            .set_subtitle(if data.release_check_available {
                "Ask GitHub for the latest Tidemark release once an hour."
            } else {
                "Release checks are disabled in this build."
            });
        self.config_path
            .set_subtitle(&display_path(&data.config_path));
        self.history_path
            .set_subtitle(&display_path(&data.history_path));
        self.history_size
            .set_subtitle(&format_bytes(data.history_bytes));
        self.key_schema.set_subtitle(&data.key_schema);
        self.token_schema.set_subtitle(&data.token_schema);
        for row in [&self.proxy_host, &self.proxy_port] {
            row.remove_css_class("error");
        }
        self.sync_proxy_editable();
        self.sync_refresh_editable();
    }

    /// Whether the manual interval row can be typed into, from what the Auto switch
    /// shows — the row's own state, not the stored preference, for the reason the proxy
    /// rows read theirs: the switch flips before the daemon answers.
    fn sync_refresh_editable(&self) {
        self.refresh_minutes
            .set_sensitive(manual_refresh_editable(self.refresh_auto.is_active()));
    }

    fn connect_switch(self: &Rc<Self>, row: &adw::SwitchRow, kind: SwitchKind) {
        row.connect_active_notify({
            let weak = Rc::downgrade(self);
            move |row| {
                let Some(settings) = weak.upgrade() else {
                    return;
                };
                if settings.suppress.get() {
                    return;
                }
                if matches!(kind, SwitchKind::RefreshAuto) {
                    // Before the round trip, so the row the mode just made irrelevant
                    // locks the moment the switch flips and not a reply later.
                    settings.sync_refresh_editable();
                }
                settings.change_switch(kind, row.is_active());
            }
        });
    }

    fn change_switch(self: Rc<Self>, kind: SwitchKind, enabled: bool) {
        let row = self.switch_row(kind).clone();
        row.set_sensitive(false);
        glib::spawn_future_local(async move {
            let result = match kind {
                SwitchKind::ReleaseCheck => self.proxy.set_release_check(enabled).await,
                SwitchKind::MinimizeOnClose => self.proxy.set_minimize_on_close(enabled).await,
                SwitchKind::RefreshAuto => {
                    let mode = refresh_mode_for(enabled);
                    self.proxy.set_refresh_mode(mode).await
                }
            };
            if let Err(error) = result {
                let preferences = self.preferences.borrow().clone();
                let data = self.data.borrow().clone();
                self.apply(&preferences, &data);
                self.toast(&error.to_string());
            } else {
                let mut preferences = self.preferences.borrow_mut();
                match kind {
                    SwitchKind::ReleaseCheck => preferences.release_check = enabled,
                    SwitchKind::MinimizeOnClose => preferences.minimize_on_close = enabled,
                    SwitchKind::RefreshAuto => {
                        preferences.refresh_mode = refresh_mode_for(enabled).to_owned();
                    }
                }
            }
            if !matches!(kind, SwitchKind::ReleaseCheck)
                || self.data.borrow().release_check_available
            {
                row.set_sensitive(true);
            }
        });
    }

    fn switch_row(&self, kind: SwitchKind) -> &adw::SwitchRow {
        match kind {
            SwitchKind::ReleaseCheck => &self.release_check,
            SwitchKind::MinimizeOnClose => &self.minimize_on_close,
            SwitchKind::RefreshAuto => &self.refresh_auto,
        }
    }

    /// Commits the manual interval each time the stepper settles on a value.
    ///
    /// The row is made insensitive for the round trip, which is what bounds the commit
    /// rate: a held stepper button cannot queue a poll per click.
    fn connect_refresh_minutes(self: &Rc<Self>) {
        self.refresh_minutes.connect_value_notify({
            let weak = Rc::downgrade(self);
            move |row| {
                let Some(settings) = weak.upgrade() else {
                    return;
                };
                if settings.suppress.get() {
                    return;
                }
                let minutes = row.value() as u32;
                row.set_sensitive(false);
                glib::spawn_future_local(async move {
                    if let Err(error) = settings.proxy.set_refresh_minutes(minutes).await {
                        let preferences = settings.preferences.borrow().clone();
                        let data = settings.data.borrow().clone();
                        settings.apply(&preferences, &data);
                        settings.toast(&error.to_string());
                    } else {
                        settings.preferences.borrow_mut().refresh_minutes = minutes;
                    }
                    // Either way the row is redrawn from the switch, which is what
                    // restores its sensitivity under the mode that allows typing.
                    settings.sync_refresh_editable();
                });
            }
        });
    }

    fn connect_startup(self: &Rc<Self>) {
        self.startup.connect_selected_notify({
            let weak = Rc::downgrade(self);
            move |row| {
                let Some(settings) = weak.upgrade() else {
                    return;
                };
                if settings.suppress.get() {
                    return;
                }
                let Some(mode) = STARTUP_VALUES.get(row.selected() as usize) else {
                    return;
                };
                let mode = (*mode).to_owned();
                row.set_sensitive(false);
                let row = row.clone();
                glib::spawn_future_local(async move {
                    if let Err(error) = settings.proxy.set_startup_mode(&mode).await {
                        let preferences = settings.preferences.borrow().clone();
                        let data = settings.data.borrow().clone();
                        settings.apply(&preferences, &data);
                        settings.toast(&error.to_string());
                    } else {
                        settings.preferences.borrow_mut().startup_mode = mode;
                    }
                    row.set_sensitive(true);
                });
            }
        });
    }

    fn connect_retention(self: &Rc<Self>) {
        self.retention.connect_selected_notify({
            let weak = Rc::downgrade(self);
            move |row| {
                let Some(settings) = weak.upgrade() else {
                    return;
                };
                if settings.suppress.get() {
                    return;
                }
                let Some(retention) = RETENTION_VALUES.get(row.selected() as usize) else {
                    return;
                };
                let retention = (*retention).to_owned();
                row.set_sensitive(false);
                let row = row.clone();
                glib::spawn_future_local(async move {
                    if let Err(error) = settings.proxy.set_history_retention(&retention).await {
                        let preferences = settings.preferences.borrow().clone();
                        let data = settings.data.borrow().clone();
                        settings.apply(&preferences, &data);
                        settings.toast(&error.to_string());
                    } else {
                        settings.preferences.borrow_mut().history_retention = retention;
                    }
                    row.set_sensitive(true);
                });
            }
        });
    }

    /// All three proxy rows commit the same way, because they are one setting.
    ///
    /// The mode opens the two rows it needs and then tries; the host and the port commit
    /// when their apply button is pressed or Enter ends the edit. Whichever of them the
    /// user touched, the daemon is sent the whole triple.
    fn connect_proxy(self: &Rc<Self>) {
        self.proxy_mode.connect_selected_notify({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(settings) = weak.upgrade()
                    && !settings.suppress.get()
                {
                    // Before the attempt, because choosing `SOCKS5` is what makes the host
                    // typeable and the attempt below is what needs it typed.
                    settings.sync_proxy_editable();
                    settings.submit_proxy(Complaint::Silent);
                }
            }
        });
        for row in [&self.proxy_host, &self.proxy_port] {
            row.connect_apply({
                let weak = Rc::downgrade(self);
                move |_| {
                    if let Some(settings) = weak.upgrade()
                        && !settings.suppress.get()
                    {
                        settings.submit_proxy(Complaint::Loud);
                    }
                }
            });
        }
    }

    /// Whether the host and the port can be typed into.
    ///
    /// The rule reads the **row** and not the stored preference, which is the whole point
    /// of it: choosing `SOCKS5` before typing where the proxy is, is how this group gets
    /// filled in, and that choice is deliberately not sent - so the stored mode is still
    /// `off` at the moment the two rows it needs have to become editable. Deriving this
    /// from storage locks them shut and leaves the setting unreachable.
    ///
    /// A mode this build cannot show empties its own row, and `selected` is then out of
    /// range: nothing to describe, nothing to type.
    fn sync_proxy_editable(&self) {
        let editable = proxy_rows_editable(self.proxy_mode.selected());
        for row in [&self.proxy_host, &self.proxy_port] {
            row.set_sensitive(editable);
        }
    }

    /// Sends the proxy the three rows currently describe.
    ///
    /// A mode with no host or no port yet is **not** sent. The daemon would refuse it and
    /// be right to, but choosing `SOCKS5` before typing where it is, is the normal way to
    /// fill this group in, and answering the first half of that with an error is answering
    /// the wrong thing: the row that still needs typing is focused, and only a deliberate
    /// submit of an incomplete one is marked as wrong.
    fn submit_proxy(self: &Rc<Self>, complaint: Complaint) {
        let Some(mode) = PROXY_VALUES.get(self.proxy_mode.selected() as usize) else {
            return;
        };
        let mode = (*mode).to_owned();
        let host = self.proxy_host.text().trim().to_owned();
        let typed = self.proxy_port.text();
        let typed = typed.trim();
        for row in [&self.proxy_host, &self.proxy_port] {
            row.remove_css_class("error");
        }
        let port = if typed.is_empty() {
            Some(0)
        } else {
            typed.parse::<u16>().ok().filter(|port| *port != 0)
        };
        let Some(port) = port else {
            self.proxy_port.add_css_class("error");
            self.proxy_port.grab_focus();
            if matches!(complaint, Complaint::Loud) {
                self.toast("A proxy port is a number from 1 to 65535");
            }
            return;
        };
        if mode != Preferences::PROXY_OFF {
            let incomplete = if host.is_empty() {
                Some(&self.proxy_host)
            } else if port == 0 {
                Some(&self.proxy_port)
            } else {
                None
            };
            if let Some(row) = incomplete {
                if matches!(complaint, Complaint::Loud) {
                    row.add_css_class("error");
                }
                row.grab_focus();
                return;
            }
        }

        let settings = Rc::clone(self);
        let rows = [
            self.proxy_host.clone().upcast::<gtk::Widget>(),
            self.proxy_port.clone().upcast(),
            self.proxy_mode.clone().upcast(),
        ];
        for row in &rows {
            row.set_sensitive(false);
        }
        glib::spawn_future_local(async move {
            let result = settings.proxy.set_proxy(&mode, &host, port).await;
            match result {
                Ok(()) => {
                    {
                        let mut preferences = settings.preferences.borrow_mut();
                        preferences.proxy_mode = mode;
                        preferences.proxy_host = host;
                        preferences.proxy_port = port;
                    }
                    settings.toast("Proxy updated");
                }
                Err(error) => settings.toast(&error.to_string()),
            }
            // Either way the rows are redrawn from the state that is now authoritative,
            // which is also what restores their sensitivity for the new mode.
            let preferences = settings.preferences.borrow().clone();
            let data = settings.data.borrow().clone();
            settings.apply(&preferences, &data);
        });
    }

    fn connect_clear(self: &Rc<Self>, button: &gtk::Button) {
        button.connect_clicked({
            let weak: Weak<Self> = Rc::downgrade(self);
            move |button| {
                let Some(settings) = weak.upgrade() else {
                    return;
                };
                button.set_sensitive(false);
                let button = button.clone();
                glib::spawn_future_local(async move {
                    let confirmation = adw::AlertDialog::builder()
                        .heading("Clear history?")
                        .body("This permanently deletes recorded quota history and notification records. Provider accounts and credentials are not affected.")
                        .build();
                    confirmation.add_responses(&[("cancel", "Cancel"), ("clear", "Clear History")]);
                    confirmation.set_default_response(Some("cancel"));
                    confirmation.set_close_response("cancel");
                    confirmation.set_response_appearance(
                        "clear",
                        adw::ResponseAppearance::Destructive,
                    );
                    if confirmation.choose_future(Some(&settings.dialog)).await == "clear" {
                        match settings.proxy.clear_history().await {
                            Ok(()) => {
                                settings.toast("History cleared");
                                if let Ok(data) = settings.proxy.get_data_info().await {
                                    let preferences = settings.preferences.borrow().clone();
                                    settings.apply(&preferences, &data);
                                }
                            }
                            Err(error) => settings.toast(&error.to_string()),
                        }
                    }
                    button.set_sensitive(true);
                });
            }
        });
    }

    fn toast(&self, message: &str) {
        self.dialog.add_toast(adw::Toast::new(message));
    }
}

fn path_row(title: &str) -> adw::ActionRow {
    adw::ActionRow::builder().title(title).build()
}

fn display_path(path: &str) -> String {
    if path.is_empty() {
        "Unavailable until the daemon is restarted".into()
    } else {
        path.into()
    }
}

/// Shows a named daemon choice on a combo row.
///
/// A value this build does not know is kept visible and untouchable rather than guessed:
/// the row is emptied of choices so it cannot show or select a different one, disabled so
/// it cannot be changed by accident, and says what the daemon actually reported. A known
/// value puts the choices back, selects its own, and leaves the row editable.
///
/// Emptying the row is what it takes: `AdwComboRow` holds a model with a valid selection
/// in it, and setting the position to [`gtk::INVALID_LIST_POSITION`] with the labels still
/// there leaves the first one selected — the very guess this avoids.
fn apply_named_choice(row: &adw::ComboRow, labels: &[&str], known: Option<u32>, raw: &str) {
    match known {
        Some(index) => {
            if row.model().is_none() {
                row.set_model(Some(&gtk::StringList::new(labels)));
            }
            row.set_use_subtitle(true);
            row.set_selected(index);
            row.set_sensitive(true);
        }
        None => {
            row.set_model(None::<&gtk::StringList>);
            row.set_use_subtitle(false);
            row.set_subtitle(&format!(
                "Unsupported value {raw:?} reported by the daemon."
            ));
            row.set_sensitive(false);
        }
    }
}

fn retention_index(value: &str) -> Option<u32> {
    RETENTION_VALUES
        .iter()
        .position(|candidate| *candidate == value)
        .map(|index| index as u32)
}

fn startup_index(value: &str) -> Option<u32> {
    STARTUP_VALUES
        .iter()
        .position(|candidate| *candidate == value)
        .map(|index| index as u32)
}

fn proxy_index(value: &str) -> Option<u32> {
    PROXY_VALUES
        .iter()
        .position(|candidate| *candidate == value)
        .map(|index| index as u32)
}

/// Whether the host and port rows belong to a proxy at all, for what the mode row has
/// selected right now - including [`gtk::INVALID_LIST_POSITION`], which is what an unknown
/// daemon value leaves behind.
fn proxy_rows_editable(selected: u32) -> bool {
    PROXY_VALUES
        .get(selected as usize)
        .is_some_and(|mode| *mode != Preferences::PROXY_OFF)
}

/// Whether the manual interval row belongs to the mode the Auto switch shows right now —
/// including the moment between a toggle and the daemon's answer.
fn manual_refresh_editable(auto_active: bool) -> bool {
    !auto_active
}

/// The named mode a switch state commits.
fn refresh_mode_for(auto_active: bool) -> &'static str {
    if auto_active {
        Preferences::REFRESH_AUTO
    } else {
        Preferences::REFRESH_MANUAL
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    match bytes {
        0..=1023 => format!("{bytes} bytes"),
        MIB..=u64::MAX if bytes < GIB => format!("{:.1} MiB", bytes as f64 / MIB as f64),
        GIB..=u64::MAX => format!("{:.1} GiB", bytes as f64 / GIB as f64),
        _ => format!("{:.1} KiB", bytes as f64 / KIB as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_retention_policy_selects_its_named_row() {
        assert_eq!(retention_index(Preferences::RETENTION_FOREVER), Some(0));
        assert_eq!(retention_index(Preferences::RETENTION_SIX_MONTHS), Some(1));
        assert_eq!(retention_index(Preferences::RETENTION_ONE_YEAR), Some(2));
        assert_eq!(retention_index("eventually"), None);
    }

    #[test]
    fn every_startup_mode_selects_its_named_row() {
        assert_eq!(startup_index(Preferences::STARTUP_APP), Some(0));
        assert_eq!(startup_index(Preferences::STARTUP_DAEMON), Some(1));
        assert_eq!(startup_index(Preferences::STARTUP_OFF), Some(2));
        assert_eq!(startup_index("everything"), None);
    }

    #[test]
    fn every_proxy_mode_selects_its_named_row() {
        assert_eq!(proxy_index(Preferences::PROXY_OFF), Some(0));
        assert_eq!(proxy_index(Preferences::PROXY_HTTP), Some(1));
        assert_eq!(proxy_index(Preferences::PROXY_HTTPS), Some(2));
        assert_eq!(proxy_index(Preferences::PROXY_SOCKS5), Some(3));
        assert_eq!(proxy_index("socks4"), None);
        assert_eq!(PROXY_VALUES.len(), PROXY_LABELS.len());
    }

    #[test]
    fn the_manual_interval_row_follows_the_auto_switch() {
        // Reads the row and not the stored preference, for the same reason the proxy
        // rows do: the switch flips before the daemon has answered, and locking the
        // row against the stored value would leave the setting unreachable mid-change.
        assert!(!manual_refresh_editable(true), "auto decides the pace");
        assert!(
            manual_refresh_editable(false),
            "manual needs a pace to read"
        );
    }

    #[test]
    fn a_switch_state_maps_back_to_one_of_the_named_refresh_modes() {
        assert_eq!(refresh_mode_for(true), Preferences::REFRESH_AUTO);
        assert_eq!(refresh_mode_for(false), Preferences::REFRESH_MANUAL);
    }

    /// The regression this exists for: the host and the port were derived from the
    /// *stored* mode, which is still `off` while a just-chosen `SOCKS5` is waiting for the
    /// host that would let it be stored. The two rows the user has to type into were the
    /// two rows that stayed locked, and the setting could not be reached at all.
    #[test]
    fn choosing_a_proxy_opens_the_rows_it_needs_before_anything_is_stored() {
        assert!(!proxy_rows_editable(0), "off has nothing to describe");
        assert!(proxy_rows_editable(1), "http needs a host and a port");
        assert!(proxy_rows_editable(2));
        assert!(proxy_rows_editable(3), "socks5 needs a host and a port");
        assert!(
            !proxy_rows_editable(gtk::INVALID_LIST_POSITION),
            "an unknown daemon mode empties its row and describes nothing"
        );
    }

    #[test]
    fn database_sizes_are_readable_without_losing_small_values() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(512), "512 bytes");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MiB");
    }
    /// Whether a display is available for building widgets; the row state below can only
    /// be observed on real ones. Every widget assertion lives in a single test because
    /// GTK belongs to the thread that initialized it and the harness gives each test its
    /// own thread.
    fn widgets() -> bool {
        static READY: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| adw::init().is_ok());
        *READY
    }

    fn choice_row(labels: &[&str]) -> adw::ComboRow {
        adw::ComboRow::builder()
            .model(&gtk::StringList::new(labels))
            .expression(gtk::PropertyExpression::new(
                gtk::StringObject::static_type(),
                None::<gtk::Expression>,
                "string",
            ))
            .use_subtitle(true)
            .build()
    }

    #[test]
    fn unknown_named_choices_are_kept_visible_and_untouchable() {
        if !widgets() {
            eprintln!("skipped: no display is available");
            return;
        }

        for (labels, known, raw) in [
            (
                &STARTUP_LABELS[..],
                startup_index(Preferences::STARTUP_DAEMON),
                "launcher",
            ),
            (
                &RETENTION_LABELS[..],
                retention_index(Preferences::RETENTION_ONE_YEAR),
                "decade",
            ),
            (
                &PROXY_LABELS[..],
                proxy_index(Preferences::PROXY_SOCKS5),
                "socks4",
            ),
        ] {
            let row = choice_row(labels);

            apply_named_choice(&row, labels, None, raw);

            assert_eq!(row.selected(), gtk::INVALID_LIST_POSITION);
            assert!(row.model().is_none());
            assert!(!row.is_sensitive());
            assert!(
                row.subtitle()
                    .is_some_and(|subtitle| subtitle.contains(&format!("{raw:?}"))),
                "the raw value should stay visible, got: {:?}",
                row.subtitle()
            );

            // A known value reported later selects its row and can be changed again.
            apply_named_choice(&row, labels, known, "ignored");
            assert_eq!(row.selected(), known.unwrap());
            assert!(row.model().is_some());
            assert!(row.is_sensitive());
            assert!(row.uses_subtitle());
        }
    }
}
