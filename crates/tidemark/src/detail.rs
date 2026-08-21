//! State and presentation for one provider's quota detail dialog.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use tidemark_types::{ProviderStatus, Timestamp, WindowStatus, provider_label};

use crate::bus::DaemonProxy;
use crate::chart::Chart;
use crate::format;
use crate::mark;

/// The window whose current segment the chart is displaying.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Selection(Option<String>);

impl Selection {
    /// Keeps a still-reported selection, otherwise chooses the same dominant window as a
    /// provider card. A status without a reading deliberately clears the old selection.
    fn apply(&mut self, status: &ProviderStatus) {
        let Some(snapshot) = status.to_snapshot() else {
            self.0 = None;
            return;
        };
        if self.0.as_deref().is_some_and(|key| {
            snapshot
                .windows
                .iter()
                .any(|window| window.key.as_str() == key)
        }) {
            return;
        }
        self.0 = snapshot
            .dominant_window()
            .map(|window| window.key.to_string());
    }

    fn key(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

/// Identifies one in-flight current-segment request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    generation: u64,
    key: String,
}

/// Makes late D-Bus replies harmless when the reader switches windows or receives an update.
#[derive(Debug, Default)]
struct RequestGeneration(u64);

impl RequestGeneration {
    fn begin(&mut self, key: &str) -> Request {
        self.0 = self.0.wrapping_add(1);
        Request {
            generation: self.0,
            key: key.into(),
        }
    }

    fn accepts(&self, request: &Request, selected: Option<&str>) -> bool {
        request.generation == self.0 && selected == Some(request.key.as_str())
    }
}

/// The one detail dialog the main window may have open.
#[derive(Debug)]
pub struct DetailDialog {
    dialog: adw::Dialog,
    proxy: DaemonProxy<'static>,
    status: RefCell<ProviderStatus>,
    selection: RefCell<Selection>,
    requests: RefCell<RequestGeneration>,
    rebuilding_windows: Cell<bool>,
    window_keys: RefCell<Vec<String>>,
    windows: gtk::ListBox,
    details: gtk::Box,
    state: gtk::Label,
    schedule: gtk::Label,
    chart: Chart,
    self_weak: RefCell<Weak<Self>>,
}

impl DetailDialog {
    /// Builds, presents, and retains one account's detail dialog.
    pub fn present(
        parent: &impl IsA<gtk::Widget>,
        proxy: DaemonProxy<'static>,
        status: ProviderStatus,
        on_closed: impl Fn() + 'static,
    ) -> Rc<Self> {
        let mark = mark::image();
        mark.set_pixel_size(32);
        mark::set(&mark, &status.provider);
        let name = gtk::Label::builder()
            .label(provider_label(&status.provider))
            .css_classes(["title-2"])
            .halign(gtk::Align::Start)
            .build();
        let state = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build();
        let identity = gtk::Box::builder().spacing(12).build();
        let identity_text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .valign(gtk::Align::Center)
            .build();
        identity_text.append(&name);
        identity_text.append(&state);
        identity.append(&mark);
        identity.append(&identity_text);

        let windows = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["boxed-list"])
            .build();
        let windows_group = adw::PreferencesGroup::builder()
            .title("Quota windows")
            .build();
        windows_group.add(&windows);

        let chart = Chart::new();
        let schedule = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["dim-label", "caption"])
            .build();
        let legend = gtk::Label::builder()
            .label("Actual · Even pace")
            .halign(gtk::Align::Start)
            .css_classes(["caption"])
            .build();
        let chart_group = adw::PreferencesGroup::builder().title("Burn-down").build();
        chart_group.add(&schedule);
        chart_group.add(&legend);
        chart_group.add(chart.widget());

        let details = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        content.append(&identity);
        content.append(&windows_group);
        content.append(&chart_group);
        content.append(&details);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&content)
            .build();
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Close")
            .build();
        let header = adw::HeaderBar::new();
        header.pack_end(&close);
        let view = adw::ToolbarView::builder().content(&scroller).build();
        view.add_top_bar(&header);
        let dialog = adw::Dialog::builder()
            .title(format!("{} details", provider_label(&status.provider)))
            .content_width(720)
            .content_height(680)
            .child(&view)
            .build();

        let detail = Rc::new(Self {
            dialog: dialog.clone(),
            proxy,
            status: RefCell::new(status),
            selection: RefCell::new(Selection::default()),
            requests: RefCell::new(RequestGeneration::default()),
            rebuilding_windows: Cell::new(false),
            window_keys: RefCell::new(Vec::new()),
            windows,
            details,
            state,
            schedule,
            chart,
            self_weak: RefCell::new(Weak::new()),
        });
        *detail.self_weak.borrow_mut() = Rc::downgrade(&detail);

        detail.windows.connect_row_selected({
            let weak = Rc::downgrade(&detail);
            move |_, row| {
                let Some(detail) = weak.upgrade() else {
                    return;
                };
                if detail.rebuilding_windows.get() {
                    return;
                }
                let Some(index) = row.map(gtk::ListBoxRow::index) else {
                    return;
                };
                let Some(key) = detail.window_keys.borrow().get(index as usize).cloned() else {
                    return;
                };
                if detail.selection.borrow().key() != Some(key.as_str()) {
                    detail.selection.borrow_mut().0 = Some(key);
                    detail.load_current_segment();
                }
            }
        });
        close.connect_clicked({
            let dialog = dialog.clone();
            move |_| {
                dialog.close();
            }
        });
        dialog.connect_closed(move |_| on_closed());

        // Keep the immutable borrow out of `apply`: it replaces the stored status as its
        // first step, so borrowing and cloning inline would overlap the two RefCell borrows.
        let initial_status = detail.status.borrow().clone();
        detail.apply(&initial_status);
        dialog.present(Some(parent));
        detail
    }

    /// Whether this dialog belongs to this exact account.
    pub fn matches(&self, provider: &str, account: &str) -> bool {
        let status = self.status.borrow();
        status.provider == provider && status.account == account
    }

    /// Refreshes the dialog after the daemon published a new status.
    pub fn apply(&self, status: &ProviderStatus) {
        *self.status.borrow_mut() = status.clone();
        self.selection.borrow_mut().apply(status);
        self.rebuild_windows();
        self.rebuild_details();
        self.update_state();
        self.load_current_segment();
    }

    /// Closes the standard dialog; its `closed` handler releases main-window ownership.
    pub fn close(&self) {
        self.dialog.close();
    }

    fn rebuild_windows(&self) {
        while let Some(child) = self.windows.first_child() {
            self.windows.remove(&child);
        }
        let status = self.status.borrow();
        *self.window_keys.borrow_mut() = status
            .windows
            .iter()
            .map(|window| window.key.clone())
            .collect();
        self.rebuilding_windows.set(true);
        for window in &status.windows {
            let row = adw::ActionRow::builder()
                .title(&window.title)
                .subtitle(window_summary(window, Timestamp::now()))
                .activatable(true)
                .build();
            row.set_cursor_from_name(Some("pointer"));
            self.windows.append(&row);
        }
        let selected = self.selection.borrow().key().map(str::to_owned);
        if let Some(index) = selected.as_deref().and_then(|key| {
            self.window_keys
                .borrow()
                .iter()
                .position(|candidate| candidate == key)
        }) {
            self.windows
                .select_row(self.windows.row_at_index(index as i32).as_ref());
        }
        self.rebuilding_windows.set(false);
    }

    fn rebuild_details(&self) {
        while let Some(child) = self.details.first_child() {
            self.details.remove(&child);
        }
        for section in self
            .status
            .borrow()
            .details
            .iter()
            .filter(|section| !section.rows.is_empty())
        {
            let group = adw::PreferencesGroup::builder()
                .title(&section.title)
                .build();
            for row in &section.rows {
                group.add(
                    &adw::ActionRow::builder()
                        .title(&row.label)
                        .subtitle(&row.value)
                        .build(),
                );
            }
            self.details.append(&group);
        }
    }

    fn update_state(&self) {
        match format::chip(&self.status.borrow()) {
            Some(chip) => {
                self.state.set_label(&chip.text);
                self.state.set_visible(true);
            }
            None => self.state.set_visible(false),
        }
    }

    fn load_current_segment(&self) {
        let status = self.status.borrow().clone();
        let Some(key) = self.selection.borrow().key().map(str::to_owned) else {
            self.schedule
                .set_label("No quota reading is available yet.");
            self.chart.set_empty("No quota reading is available yet.");
            return;
        };
        let Some(window) = status
            .windows
            .iter()
            .find(|window| window.key == key)
            .cloned()
        else {
            self.schedule
                .set_label("The selected window is no longer reported.");
            self.chart
                .set_empty("The selected window is no longer reported.");
            return;
        };
        self.schedule.set_label(&schedule_text(&window));
        self.chart.set_loading();
        let request = self.requests.borrow_mut().begin(&key);
        let proxy = self.proxy.clone();
        let weak = self.self_weak.borrow().clone();
        gtk::glib::spawn_future_local(async move {
            let result = proxy
                .current_segment(&status.provider, &status.account, &request.key)
                .await;
            let Some(detail) = weak.upgrade() else {
                return;
            };
            if !detail
                .requests
                .borrow()
                .accepts(&request, detail.selection.borrow().key())
            {
                return;
            }
            match result {
                Ok(points) => detail.chart.set_data(window, points),
                Err(error) => detail
                    .chart
                    .set_error(&format!("Could not load history: {error}")),
            }
        });
    }
}

fn window_summary(window: &WindowStatus, now: Timestamp) -> String {
    let mut summary = format::percent(window.used_percent);
    if let Some(reset) = window
        .resets_at
        .and_then(|seconds| Timestamp::from_unix(seconds).ok())
    {
        summary.push_str(" · ");
        summary.push_str(&format::resets_in(now.seconds_until(reset)));
    }
    summary
}

fn schedule_text(window: &WindowStatus) -> String {
    match (window.resets_at, window.length_secs) {
        (Some(_), Some(_)) => "Actual consumption compared with even pace.".into(),
        _ => "Schedule unavailable; showing actual consumption only.".into(),
    }
}

#[cfg(test)]
mod tests {
    use tidemark_types::{AccountId, ProviderId, ProviderStatus, WindowStatus};

    use super::{RequestGeneration, Selection};

    fn status(windows: &[(&str, Option<u64>)]) -> ProviderStatus {
        let mut status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        status.captured_at = Some(1_785_700_000);
        status.windows = windows
            .iter()
            .map(|(key, length_secs)| WindowStatus {
                key: (*key).into(),
                title: (*key).into(),
                used_percent: 42.0,
                resets_at: Some(1_785_718_000),
                length_secs: *length_secs,
            })
            .collect();
        status
    }

    #[test]
    fn initial_selection_is_the_dominant_window() {
        let mut selection = Selection::default();
        selection.apply(&status(&[
            ("weekly", Some(604_800)),
            ("five-hour", Some(18_000)),
        ]));
        assert_eq!(selection.key(), Some("five-hour"));
    }

    #[test]
    fn an_existing_selected_window_survives_a_live_status_update() {
        let mut selection = Selection::default();
        selection.apply(&status(&[
            ("weekly", Some(604_800)),
            ("five-hour", Some(18_000)),
        ]));
        selection.apply(&status(&[
            ("monthly", Some(2_592_000)),
            ("five-hour", Some(18_000)),
        ]));
        assert_eq!(selection.key(), Some("five-hour"));
    }

    #[test]
    fn a_disappeared_selected_window_falls_back_to_the_new_dominant_one() {
        let mut selection = Selection::default();
        selection.apply(&status(&[
            ("weekly", Some(604_800)),
            ("five-hour", Some(18_000)),
        ]));
        selection.apply(&status(&[
            ("weekly", Some(604_800)),
            ("monthly", Some(2_592_000)),
        ]));
        assert_eq!(selection.key(), Some("weekly"));
    }

    #[test]
    fn a_status_without_a_reading_has_no_selection() {
        let mut selection = Selection::default();
        selection.apply(&status(&[("five-hour", Some(18_000))]));
        selection.apply(&ProviderStatus::pending(
            &ProviderId::new("zai"),
            &AccountId::default(),
        ));
        assert_eq!(selection.key(), None);
    }

    #[test]
    fn an_old_history_reply_cannot_replace_a_newer_selection() {
        let mut requests = RequestGeneration::default();
        let old = requests.begin("five-hour");
        let current = requests.begin("weekly");

        assert!(!requests.accepts(&old, Some("weekly")));
        assert!(requests.accepts(&current, Some("weekly")));
    }
}
