//! One account's quota history and published details.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};
use tidemark_types::{DetailSection, HistoryPoint, ProviderStatus, Timestamp, WindowStatus};

use crate::bus::DaemonProxy;
use crate::window::spawn;
use crate::{
    AppWindow, QuotaDetailRow, QuotaDetails, QuotaSectionData, QuotaWindowData, chart, format,
};

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
    fn invalidate(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }

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

type Daemon = Rc<dyn Fn() -> Option<DaemonProxy<'static>>>;

/// The one account dialog retained by the main window. Every load asks for the current
/// daemon, so reconnecting does not leave a dead proxy behind in an open dialog.
pub struct DetailDialog {
    ui: slint::Weak<AppWindow>,
    daemon: Daemon,
    status: RefCell<ProviderStatus>,
    selection: RefCell<Selection>,
    requests: RefCell<RequestGeneration>,
    closed: Cell<bool>,
    on_closed: Box<dyn Fn()>,
}

impl DetailDialog {
    pub fn open(
        ui: &AppWindow,
        daemon: Daemon,
        status: &ProviderStatus,
        name: &str,
        mark: Option<slint::Image>,
        on_closed: impl Fn() + 'static,
    ) -> Rc<Self> {
        let detail = Rc::new(Self {
            ui: ui.as_weak(),
            daemon,
            status: RefCell::new(status.clone()),
            selection: RefCell::default(),
            requests: RefCell::default(),
            closed: Cell::new(false),
            on_closed: Box::new(on_closed),
        });
        let global = ui.global::<QuotaDetails>();
        let weak = Rc::downgrade(&detail);
        global.on_close(move || {
            if let Some(detail) = weak.upgrade() {
                detail.close();
            }
        });
        let weak = Rc::downgrade(&detail);
        global.on_select(move |index| {
            if let (Some(detail), Ok(index)) = (weak.upgrade(), usize::try_from(index)) {
                detail.select(index);
            }
        });
        detail.apply(status, name, mark);
        global.set_open(true);
        detail
    }

    pub fn matches(&self, provider: &str, account: &str) -> bool {
        let status = self.status.borrow();
        status.provider == provider && status.account == account
    }

    pub fn apply(self: &Rc<Self>, status: &ProviderStatus, name: &str, mark: Option<slint::Image>) {
        if self.closed.get() {
            return;
        }
        self.status.replace(status.clone());
        self.selection.borrow_mut().apply(status);
        if let Some(ui) = self.ui.upgrade() {
            let global = ui.global::<QuotaDetails>();
            global.set_name(name.into());
            global.set_account(if status.account == "default" {
                status.account_label.as_deref().unwrap_or("").into()
            } else {
                status
                    .account_label
                    .as_deref()
                    .unwrap_or(&status.account)
                    .into()
            });
            global.set_has_mark(mark.is_some());
            global.set_mark(mark.unwrap_or_default());
            global.set_state(
                format::chip(status)
                    .map(|chip| chip.text)
                    .unwrap_or_default()
                    .into(),
            );
            global.set_sections(ModelRc::new(VecModel::from(
                detail_sections(status)
                    .into_iter()
                    .map(|section| QuotaSectionData {
                        title: section.title.into(),
                        rows: ModelRc::new(VecModel::from(
                            section
                                .rows
                                .into_iter()
                                .map(|row| QuotaDetailRow {
                                    label: row.label.into(),
                                    value: row.value.into(),
                                })
                                .collect::<Vec<_>>(),
                        )),
                    })
                    .collect::<Vec<_>>(),
            )));
        }
        self.tick();
        self.load();
    }

    /// Clock-dependent summaries change without requesting the same history every tick.
    pub fn tick(&self) {
        if self.closed.get() {
            return;
        }
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let global = ui.global::<QuotaDetails>();
        let status = self.status.borrow();
        let now = Timestamp::now();
        global.set_windows(ModelRc::new(VecModel::from(
            status
                .windows
                .iter()
                .map(|window| QuotaWindowData {
                    title: window.title.clone().into(),
                    summary: window_summary(window, now).into(),
                })
                .collect::<Vec<_>>(),
        )));
        global.set_selected(
            status
                .windows
                .iter()
                .position(|window| self.selection.borrow().key() == Some(window.key.as_str()))
                .map_or(-1, |index| index as i32),
        );
    }

    fn select(self: &Rc<Self>, index: usize) {
        if self.closed.get() {
            return;
        }
        let key = self
            .status
            .borrow()
            .windows
            .get(index)
            .map(|window| window.key.clone());
        if let Some(key) = key
            && self.selection.borrow().key() != Some(key.as_str())
        {
            self.selection.borrow_mut().0 = Some(key);
            self.tick();
            self.load();
        }
    }

    pub fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.requests.borrow_mut().invalidate();
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<QuotaDetails>().set_open(false);
        }
        (self.on_closed)();
    }

    pub fn disconnected(&self) {
        self.requests.borrow_mut().invalidate();
        self.message(
            "The daemon is unavailable. History will reload when it reconnects.",
            false,
            true,
        );
    }

    fn message(&self, message: &str, loading: bool, failed: bool) {
        if let Some(ui) = self.ui.upgrade() {
            let global = ui.global::<QuotaDetails>();
            global.set_actual("".into());
            global.set_has_marker(false);
            global.set_message(message.into());
            global.set_loading(loading);
            global.set_failed(failed);
        }
    }

    fn load(self: &Rc<Self>) {
        self.requests.borrow_mut().invalidate();
        let status = self.status.borrow().clone();
        let selected = self.selection.borrow().key().map(str::to_owned);
        let window = status
            .windows
            .iter()
            .find(|window| Some(window.key.as_str()) == selected.as_deref())
            .cloned();
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let global = ui.global::<QuotaDetails>();
        global.set_schedule(
            window
                .as_ref()
                .map(schedule_text)
                .unwrap_or_default()
                .into(),
        );
        global.set_has_schedule(
            window
                .as_ref()
                .is_some_and(|window| window.resets_at.is_some() && window.length_secs.is_some()),
        );
        let Some(window) = window else {
            self.message("No quota reading is available yet.", false, false);
            return;
        };
        let Some(proxy) = (self.daemon)() else {
            self.disconnected();
            return;
        };
        self.message("Loading current segment…", true, false);
        let request = self.requests.borrow_mut().begin(&window.key);
        let weak = Rc::downgrade(self);
        spawn(async move {
            let result = proxy
                .current_segment(&status.provider, &status.account, &request.key)
                .await;
            let Some(detail) = weak.upgrade() else {
                return;
            };
            if detail.closed.get()
                || !detail
                    .requests
                    .borrow()
                    .accepts(&request, detail.selection.borrow().key())
            {
                return;
            }
            match result {
                Ok(points) => detail.show_chart(&window, &points),
                Err(error) => {
                    detail.message(&format!("Could not load history: {error}"), false, true)
                }
            }
        });
    }

    fn show_chart(&self, window: &WindowStatus, points: &[HistoryPoint]) {
        let geometry = chart::geometry(window, points, 1000.0, 1000.0);
        if geometry.actual.is_empty() {
            self.message("No stored readings in this segment yet.", false, false);
            return;
        }
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let global = ui.global::<QuotaDetails>();
        let mut path = String::new();
        use std::fmt::Write;
        for (index, point) in geometry.actual.iter().enumerate() {
            write!(
                &mut path,
                "{} {} {} ",
                if index == 0 { 'M' } else { 'L' },
                point.x,
                point.y
            )
            .expect("writing to a String");
        }
        global.set_actual(path.into());
        global.set_has_marker(geometry.marker.is_some());
        if let Some(marker) = geometry.marker {
            global.set_marker_x(marker.x as f32);
            global.set_marker_y(marker.y as f32);
        }
        global.set_has_schedule(geometry.diagonal.is_some());
        global.set_message("".into());
        global.set_loading(false);
        global.set_failed(false);
    }
}

/// Preserve section boundaries as well as the producer's item order.
fn detail_sections(status: &ProviderStatus) -> Vec<DetailSection> {
    let Some(presentation) = &status.presentation else {
        return status
            .details
            .iter()
            .filter(|section| !section.rows.is_empty())
            .cloned()
            .collect();
    };
    presentation
        .details
        .iter()
        .filter_map(|section| {
            let rows = section
                .items
                .iter()
                .filter_map(|widget| crate::view::detail_row(presentation, widget))
                .collect::<Vec<_>>();
            (!rows.is_empty()).then(|| DetailSection {
                title: section.title.clone(),
                rows,
            })
        })
        .collect()
}

#[cfg(test)]
fn detail_rows(status: &ProviderStatus) -> Vec<(String, String)> {
    detail_sections(status)
        .into_iter()
        .flat_map(|section| section.rows.into_iter().map(|row| (row.label, row.value)))
        .collect()
}

/// The one line under a window's title: how much is gone, the absolute quantities behind
/// that percentage when the provider sent them, and when the window resets.
///
/// The absolutes sit next to the percentage they qualify rather than at the end, and they
/// are the provider's own words — set, never parsed and never reformatted. Each piece after
/// the percentage is omitted when it is absent, so a provider that reports only a
/// percentage produces the same line it always did.
fn window_summary(window: &WindowStatus, now: Timestamp) -> String {
    let mut summary = tidemark_types::present::percent(window.used_percent);
    if let Some(subtitle) = window.subtitle.as_deref() {
        summary.push_str(" · ");
        summary.push_str(subtitle);
    }
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
    use tidemark_types::{AccountId, ProviderId, ProviderStatus, Timestamp, WindowStatus};

    use super::{RequestGeneration, Selection, window_summary};

    #[test]
    fn detail_sections_come_from_the_presentation_in_its_own_order() {
        use tidemark_types::{Field, Metric, Presentation, PresentedSection, Widget};
        let mut status = status(&[]);
        let metric = |id: &str, title: &str| Metric {
            id: id.into(),
            title: title.into(),
            subtitle: None,
            value: Some(12.5),
            maximum: Some(50.0),
            remaining: None,
            used_percent: None,
            text: Some("active".into()),
            unit: Some("USD".into()),
            window: None,
        };
        status.presentation = Some(Presentation {
            metrics: vec![metric("a", "Monthly cost"), metric("b", "Subscription")],
            card: vec![],
            details: vec![
                PresentedSection {
                    title: "Second".into(),
                    items: vec![Widget::status("b"), Widget::value("ghost", Field::Value)],
                },
                PresentedSection {
                    title: "First".into(),
                    items: vec![
                        Widget::ratio("a", Field::Value, Field::Maximum),
                        Widget::value("a", Field::Remaining),
                    ],
                },
            ],
        });
        assert_eq!(
            super::detail_rows(&status),
            [
                ("Subscription".to_owned(), "active".to_owned()),
                ("Monthly cost".to_owned(), "12.5 of 50 USD".to_owned())
            ]
        );
        assert_eq!(
            super::detail_sections(&status)
                .iter()
                .map(|section| section.title.as_str())
                .collect::<Vec<_>>(),
            ["Second", "First"]
        );
    }

    #[test]
    fn legacy_details_are_used_only_when_presentation_is_absent() {
        use tidemark_types::{DetailRow, DetailSection, Presentation};
        let mut status = status(&[]);
        status.details = vec![DetailSection {
            title: "Legacy".into(),
            rows: vec![DetailRow {
                label: "Plan".into(),
                value: "Pro".into(),
            }],
        }];
        assert_eq!(super::detail_rows(&status), [("Plan".into(), "Pro".into())]);
        status.presentation = Some(Presentation {
            metrics: vec![],
            card: vec![],
            details: vec![],
        });
        assert!(super::detail_rows(&status).is_empty());
    }

    fn status(windows: &[(&str, Option<u64>)]) -> ProviderStatus {
        let mut status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        status.captured_at = Some(1_785_700_000);
        status.windows = windows
            .iter()
            .map(|(key, length_secs)| WindowStatus {
                key: (*key).into(),
                title: (*key).into(),
                subtitle: None,
                used_percent: 42.0,
                resets_at: Some(1_785_718_000),
                length_secs: *length_secs,
                blocked_by: None,
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

    #[test]
    fn reconnects_and_updates_of_the_same_window_invalidate_old_history() {
        let mut requests = RequestGeneration::default();
        let old = requests.begin("weekly");
        requests.invalidate();
        assert!(!requests.accepts(&old, Some("weekly")));
        let current = requests.begin("weekly");
        assert!(!requests.accepts(&old, Some("weekly")));
        assert!(requests.accepts(&current, Some("weekly")));
        assert!(!requests.accepts(&current, None));
    }

    #[cfg(unix)]
    #[test]
    fn the_dialog_loads_the_selected_accounts_history_and_reports_empty_or_failed_replies() {
        use super::*;
        use slint::ComponentHandle;
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        use slint::platform::{EventLoopProxy, Platform, PlatformError, WindowAdapter};
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        type Job = Box<dyn FnOnce() + Send>;
        #[derive(Clone, Default)]
        struct Queue(Arc<Mutex<VecDeque<Job>>>);
        impl Queue {
            fn until(&self, done: impl Fn() -> bool) {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !done() {
                    let job = self.0.lock().unwrap().pop_front();
                    if let Some(job) = job {
                        job();
                    } else {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    assert!(
                        Instant::now() < deadline,
                        "headless history request timed out"
                    );
                }
            }
        }
        impl EventLoopProxy for Queue {
            fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
                Ok(())
            }
            fn invoke_from_event_loop(&self, event: Job) -> Result<(), slint::EventLoopError> {
                self.0.lock().unwrap().push_back(event);
                Ok(())
            }
        }
        impl Platform for Queue {
            fn create_window_adapter(
                &self,
            ) -> Result<std::rc::Rc<dyn WindowAdapter>, PlatformError> {
                Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
            }
            fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
                Some(Box::new(self.clone()))
            }
        }
        struct Call {
            provider: String,
            account: String,
            key: String,
            reply: async_channel::Sender<zbus::fdo::Result<Vec<HistoryPoint>>>,
        }
        struct History(async_channel::Sender<Call>);
        #[zbus::interface(name = "io.github.zbndev.Tidemark.Daemon1")]
        impl History {
            async fn current_segment(
                &self,
                provider: &str,
                account: &str,
                window: &str,
            ) -> zbus::fdo::Result<Vec<HistoryPoint>> {
                let (reply, result) = async_channel::bounded(1);
                self.0
                    .send(Call {
                        provider: provider.into(),
                        account: account.into(),
                        key: window.into(),
                        reply,
                    })
                    .await
                    .unwrap();
                result.recv().await.unwrap()
            }
        }

        // A socket pair and a software adapter: no session bus, panel, OS window or GUI.
        let (server_stream, client_stream) = std::os::unix::net::UnixStream::pair().unwrap();
        let (calls, received) = async_channel::unbounded();
        let server = std::thread::spawn(move || {
            async_io::block_on(async {
                zbus::connection::Builder::async_io_unix_stream(server_stream)
                    .server(zbus::Guid::generate())
                    .unwrap()
                    .p2p()
                    .serve_at(tidemark_types::ids::OBJECT_PATH, History(calls))
                    .unwrap()
                    .build()
                    .await
                    .unwrap()
            })
        });
        let client = async_io::block_on(
            zbus::connection::Builder::async_io_unix_stream(client_stream)
                .p2p()
                .build(),
        )
        .unwrap();
        let _server = server.join().unwrap();
        let proxy = async_io::block_on(DaemonProxy::new(&client)).unwrap();
        let daemon = Rc::new(RefCell::new(Some(proxy)));
        let queue = Queue::default();
        slint::platform::set_platform(Box::new(queue.clone())).unwrap();
        let ui = AppWindow::new().unwrap();
        let mut reading = status(&[("weekly", Some(604_800)), ("five-hour", Some(18_000))]);
        reading.account = "second".into();
        reading.account_label = Some("Work".into());
        let dialog = DetailDialog::open(
            &ui,
            Rc::new({
                let daemon = daemon.clone();
                move || daemon.borrow().clone()
            }),
            &reading,
            "Z.ai",
            None,
            || {},
        );
        let global = ui.global::<QuotaDetails>();
        assert!(global.get_open());
        assert_eq!(global.get_account(), "Work");
        assert_eq!(global.get_selected(), 1);
        assert!(global.get_loading());
        queue.until(|| !received.is_empty());
        let call = received.try_recv().unwrap();
        assert_eq!(
            (
                call.provider.as_str(),
                call.account.as_str(),
                call.key.as_str()
            ),
            ("zai", "second", "five-hour")
        );
        call.reply
            .try_send(Ok(vec![HistoryPoint {
                captured_at: 1_785_700_000,
                used_percent: 25.0,
            }]))
            .unwrap();
        queue.until(|| !global.get_loading());
        assert!(global.get_message().is_empty());
        assert!(!global.get_actual().is_empty());
        assert!(global.get_has_marker());
        assert_eq!(global.get_marker_y(), 750.0);

        global.invoke_select(0);
        queue.until(|| !received.is_empty());
        let call = received.try_recv().unwrap();
        assert_eq!(call.key, "weekly");
        call.reply.try_send(Ok(Vec::new())).unwrap();
        queue.until(|| !global.get_loading());
        assert!(!global.get_message().is_empty());
        assert!(!global.get_failed());
        assert!(global.get_actual().is_empty());
        assert!(!global.get_has_marker());

        dialog.apply(&reading, "Z.ai", None);
        assert_eq!(
            global.get_selected(),
            0,
            "a status update preserves selection"
        );
        queue.until(|| !received.is_empty());
        received
            .try_recv()
            .unwrap()
            .reply
            .try_send(Err(zbus::fdo::Error::Failed("history unavailable".into())))
            .unwrap();
        queue.until(|| !global.get_loading());
        assert!(global.get_failed());
        assert!(global.get_message().contains("history unavailable"));

        daemon.borrow_mut().take();
        dialog.disconnected();
        assert!(global.get_failed());
        assert!(!global.get_loading());
        global.invoke_close();
        assert!(!global.get_open());
        dialog.apply(&reading, "Z.ai", None);
        assert!(
            !global.get_open(),
            "late updates cannot resurrect a closed dialog"
        );
    }

    #[test]
    fn a_windows_line_carries_the_absolutes_the_provider_sent_beside_its_percentage() {
        let now = Timestamp::from_unix(1_785_700_000).expect("plausible");
        let mut window = status(&[("five-hour", Some(18_000))]).windows.remove(0);
        assert_eq!(window_summary(&window, now), "42% · resets in 5 h");

        window.subtitle = Some("420 / 1000 credits".to_owned());
        assert_eq!(
            window_summary(&window, now),
            "42% · 420 / 1000 credits · resets in 5 h"
        );
    }

    #[test]
    fn a_window_reporting_only_a_percentage_keeps_the_line_it_always_had() {
        let now = Timestamp::from_unix(1_785_700_000).expect("plausible");
        let mut window = status(&[("five-hour", Some(18_000))]).windows.remove(0);
        window.resets_at = None;

        assert_eq!(window_summary(&window, now), "42%");
    }
}
