//! Application preferences: behavior, startup, updates and local data.

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

#[derive(Debug, Clone, Copy)]
enum SwitchKind {
    ReleaseCheck,
    MinimizeOnClose,
}

/// The standard libadwaita Preferences dialog and its authoritative daemon-backed state.
#[derive(Debug)]
pub struct PreferencesDialog {
    dialog: adw::PreferencesDialog,
    proxy: DaemonProxy<'static>,
    release_check: adw::SwitchRow,
    minimize_on_close: adw::SwitchRow,
    startup: adw::ComboRow,
    retention: adw::ComboRow,
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
        let general = adw::PreferencesPage::builder()
            .title("General")
            .icon_name("preferences-system-symbolic")
            .build();
        general.add(&behavior);
        general.add(&startup);
        dialog.add(&general);

        let release_check = adw::SwitchRow::builder()
            .title("Check for updates")
            .subtitle("Ask GitHub for the latest Tidemark release once an hour.")
            .build();
        let release_group = adw::PreferencesGroup::builder()
            .title("Release Updates")
            .build();
        release_group.add(&release_check);
        let updates = adw::PreferencesPage::builder()
            .title("Updates")
            .icon_name("software-update-available-symbolic")
            .build();
        updates.add(&release_group);
        dialog.add(&updates);

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
            startup: startup_mode,
            retention,
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
        settings.connect_startup();
        settings.connect_retention();
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
        }
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
                &STARTUP_LABELS,
                startup_index(Preferences::STARTUP_DAEMON),
                "launcher",
            ),
            (
                &RETENTION_LABELS,
                retention_index(Preferences::RETENTION_ONE_YEAR),
                "decade",
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
