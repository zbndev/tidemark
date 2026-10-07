//! Questions asked through the window's alert dialog, answered by awaiting them — what
//! `AdwAlertDialog::choose_future` is to the GTK client.
//!
//! One question is on screen at a time. A second one replaces the first, which is answered
//! with its close response, as if it had been dismissed.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use slint::{ComponentHandle, ModelRc, VecModel};
use tidemark_types::account_id_suggestion;

use crate::provider_settings::{name_suggests_usable, plugins};
use crate::{Alert, AlertForm, AlertResponse, AppWindow, PreviewRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Plain,
    Suggested,
    Destructive,
}

#[derive(Debug)]
pub enum Content {
    None,
    /// A diagnostic that may be longer than the window; selectable and scrollable.
    Details,
    /// An account name, previewed as the id it suggests.
    Name {
        prefill: String,
        /// A rename: the id the account already has, which the name may not suggest again.
        current: Option<String>,
    },
    /// Everything one plugin account needs: its name, where its key is sent, and the key.
    PluginAccount {
        ask_name: bool,
        ask_endpoint: bool,
        request: String,
    },
    /// What a plugin file declares, before it is installed.
    Preview {
        rows: Vec<(String, String)>,
        mark: Option<slint::Image>,
    },
}

#[derive(Debug)]
pub struct Question {
    pub heading: String,
    pub body: String,
    pub responses: Vec<(&'static str, &'static str, Appearance)>,
    pub default: &'static str,
    pub close: &'static str,
    pub content: Content,
}

impl Question {
    /// Cancel and one destructive response, Cancel the default — a removal's confirmation.
    pub fn destructive(heading: String, body: &str, confirm: &'static str) -> Self {
        Self {
            heading,
            body: body.to_owned(),
            responses: vec![
                ("cancel", "Cancel", Appearance::Plain),
                ("confirm", confirm, Appearance::Destructive),
            ],
            default: "cancel",
            close: "cancel",
            content: Content::None,
        }
    }

    /// Cancel and one suggested response, the suggested one the default.
    pub fn suggested(heading: String, confirm: &'static str, content: Content) -> Self {
        Self {
            heading,
            body: String::new(),
            responses: vec![
                ("cancel", "Cancel", Appearance::Plain),
                ("confirm", confirm, Appearance::Suggested),
            ],
            default: "confirm",
            close: "cancel",
            content,
        }
    }

    /// A refusal to read and close.
    pub fn notice(heading: &str, body: String) -> Self {
        Self {
            heading: heading.to_owned(),
            body,
            responses: vec![("close", "Close", Appearance::Plain)],
            default: "close",
            close: "close",
            content: Content::None,
        }
    }
}

#[derive(Debug)]
pub struct Answer {
    pub response: String,
    pub form: AlertForm,
}

impl Answer {
    pub fn confirmed(&self) -> bool {
        self.response == "confirm"
    }

    /// The id the typed name suggests: what a new or renamed account is stored as.
    pub fn account_id(&self) -> String {
        account_id_suggestion(&self.form.name)
    }
}

#[derive(Debug, Default)]
struct Slot {
    answer: Option<Answer>,
    waker: Option<Waker>,
}

#[derive(Debug)]
struct Pending {
    slot: Rc<RefCell<Slot>>,
    close: &'static str,
}

pub struct Alerts {
    ui: slint::Weak<AppWindow>,
    pending: RefCell<Option<Pending>>,
    serial: Cell<i32>,
}

impl std::fmt::Debug for Alerts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Alerts")
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

impl Alerts {
    pub fn install(ui: &AppWindow) -> Rc<Self> {
        let alerts = Rc::new(Self {
            ui: ui.as_weak(),
            pending: RefCell::new(None),
            serial: Cell::new(0),
        });
        let alert = ui.global::<Alert>();
        alert.on_respond({
            let weak = Rc::downgrade(&alerts);
            move |response, form| {
                if let Some(alerts) = weak.upgrade() {
                    alerts.answer(response.into(), form);
                }
            }
        });
        alert.on_filled(|text| !text.trim().is_empty());
        alert.on_suggest_id(|name| account_id_suggestion(&name).into());
        alert.on_name_usable(|name, current| {
            name_suggests_usable(&name, (!current.is_empty()).then_some(current.as_str()))
        });
        alert.on_endpoint_valid(|url| plugins::valid_endpoint(&url));
        alert.on_endpoint_warning(|url| plugins::endpoint_warning(&url).unwrap_or_default().into());
        alerts
    }

    pub fn ask(&self, question: Question) -> impl Future<Output = Answer> + use<> {
        // A question still on screen is dismissed by the one replacing it.
        if let Some(earlier) = self.pending.borrow().as_ref() {
            resolve(
                &earlier.slot,
                earlier.close.to_owned(),
                AlertForm::default(),
            );
        }
        let slot = Rc::new(RefCell::new(Slot::default()));
        self.pending.replace(Some(Pending {
            slot: Rc::clone(&slot),
            close: question.close,
        }));
        self.show(question);
        Answered(slot)
    }

    fn show(&self, question: Question) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let alert = ui.global::<Alert>();
        self.serial.set(self.serial.get() + 1);
        alert.set_heading(question.heading.into());
        alert.set_body(question.body.into());
        let default_suggested = question.responses.iter().any(|(id, _, appearance)| {
            *id == question.default && *appearance == Appearance::Suggested
        });
        let responses: Vec<AlertResponse> = question
            .responses
            .iter()
            .map(|(id, label, appearance)| AlertResponse {
                id: (*id).into(),
                label: (*label).into(),
                kind: match appearance {
                    Appearance::Plain => 0,
                    Appearance::Suggested => 1,
                    Appearance::Destructive => 2,
                },
            })
            .collect();
        alert.set_responses(ModelRc::new(VecModel::from(responses)));
        alert.set_default_response(question.default.into());
        alert.set_default_suggested(default_suggested);
        alert.set_close_response(question.close.into());
        alert.set_prefill("".into());
        alert.set_current("".into());
        alert.set_has_mark(false);
        match question.content {
            Content::None => alert.set_content(0),
            Content::Details => alert.set_content(4),
            Content::Name { prefill, current } => {
                alert.set_content(1);
                alert.set_prefill(prefill.into());
                alert.set_current(current.unwrap_or_default().into());
            }
            Content::PluginAccount {
                ask_name,
                ask_endpoint,
                request,
            } => {
                alert.set_content(2);
                alert.set_ask_name(ask_name);
                alert.set_ask_endpoint(ask_endpoint);
                alert.set_request(request.into());
            }
            Content::Preview { rows, mark } => {
                alert.set_content(3);
                let rows: Vec<PreviewRow> = rows
                    .into_iter()
                    .map(|(label, value)| PreviewRow {
                        label: label.into(),
                        value: value.into(),
                    })
                    .collect();
                alert.set_preview(ModelRc::new(VecModel::from(rows)));
                alert.set_has_mark(mark.is_some());
                alert.set_mark(mark.unwrap_or_default());
            }
        }
        alert.set_serial(self.serial.get());
        alert.set_open(true);
    }

    fn answer(&self, response: String, form: AlertForm) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<Alert>().set_open(false);
        }
        resolve(&pending.slot, response, form);
    }
}

fn resolve(slot: &RefCell<Slot>, response: String, form: AlertForm) {
    let mut slot = slot.borrow_mut();
    if slot.answer.is_some() {
        return;
    }
    slot.answer = Some(Answer { response, form });
    if let Some(waker) = slot.waker.take() {
        waker.wake();
    }
}

struct Answered(Rc<RefCell<Slot>>);

impl Future for Answered {
    type Output = Answer;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Answer> {
        let mut slot = self.0.borrow_mut();
        match slot.answer.take() {
            Some(answer) => Poll::Ready(answer),
            None => {
                slot.waker = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }
}
