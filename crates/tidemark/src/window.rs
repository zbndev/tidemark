//! The window: a grid of cards, and the two things that can be there instead of it.
//!
//! `GtkFlowBox` does the columns, between one and three by width, as `CONTEXT.md`
//! § Interface asks. Its known cost is the last row: with four cards in a three-wide grid,
//! the fourth sits alone. That is accepted rather than papered over — a filler card would
//! be an invitation to click on nothing — while *within* a row the heights are equalised,
//! which `GtkFlowBox` does for free by stretching every child to the tallest, and which the
//! card turns into something deliberate by pinning its footer to the bottom.
//!
//! The window is also where "updates without user action" is made true: statuses arrive on
//! a signal, and a timer redraws the parts that depend on the clock rather than on new
//! data — the relative timestamps, and the pace marks, which keep moving while nothing else
//! changes.

use std::cell::RefCell;
use std::cmp::Ordering;

use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;
use tidemark_types::{ProviderStatus, Timestamp};

use crate::bus::{self, DaemonProxy, Update};
use crate::card::Card;
use crate::credentials::Credentials;
use crate::model;

/// How often the clock-dependent parts of every card are redrawn. Half a minute is well
/// inside the resolution of everything shown — the coarsest unit on a card is a minute —
/// and costs a redraw of a handful of labels.
const TICK_SECONDS: u32 = 30;

/// Names of the pages in the stack.
const PAGE_GRID: &str = "grid";
const PAGE_MESSAGE: &str = "message";

/// The main window and everything it is currently showing.
#[derive(Debug)]
pub struct MainWindow {
    window: adw::ApplicationWindow,
    stack: gtk::Stack,
    message: adw::StatusPage,
    grid: gtk::FlowBox,
    refresh: gtk::Button,
    providers: gtk::Button,
    cards: RefCell<Vec<Rc<Card>>>,
    daemon: RefCell<Option<DaemonProxy<'static>>>,
    /// The credentials dialog while it is on screen, so the statuses arriving on the
    /// signal reach it too. A dialog that went on saying "no key" after the key was
    /// accepted would be the one place in the program that lied about the daemon.
    credentials: RefCell<Option<Rc<Credentials>>>,
}

impl MainWindow {
    /// Builds the window, connects it to the daemon and presents it.
    pub fn present(app: &adw::Application) {
        let grid = gtk::FlowBox::builder()
            .min_children_per_line(1)
            .max_children_per_line(3)
            .homogeneous(true)
            .row_spacing(12)
            .column_spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .valign(gtk::Align::Start)
            // Centred rather than stretched: `GtkFlowBox` gives every child the width of
            // one column whatever the window is doing, so a filled row and a half-empty one
            // both sit under the middle of the window instead of hugging its left edge.
            .halign(gtk::Align::Center)
            // Cards are not a list to pick from; the click that will matter opens a detail
            // dialog, and that is the card's own gesture rather than a selection.
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["quota-grid"])
            .build();

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
        let providers = gtk::Button::builder()
            .icon_name("dialog-password-symbolic")
            .tooltip_text("Providers and credentials")
            .sensitive(false)
            .build();

        let header = adw::HeaderBar::new();
        header.pack_end(&refresh);
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

        let main = Rc::new(Self {
            window,
            stack,
            message,
            grid,
            refresh,
            providers,
            cards: RefCell::new(Vec::new()),
            daemon: RefCell::new(None),
            credentials: RefCell::new(None),
        });

        main.install_sort();
        main.connect_refresh_button();
        main.connect_providers_button();
        main.start_clock();
        main.window.present();

        // The strong reference lives here, in the closure the bus watcher keeps for the
        // life of the process. GTK owns the widgets, but nothing owns the Rust state that
        // drives them, and a weak reference here would be dead the moment this function
        // returned — leaving a window that connects to the daemon and then ignores every
        // word it says.
        bus::watch(move |update| main.handle(update));
    }

    /// Acts on one message from the daemon.
    fn handle(&self, update: Update) {
        match update {
            Update::Connected(proxy, statuses) => {
                *self.daemon.borrow_mut() = Some(proxy);
                self.refresh.set_sensitive(true);
                self.providers.set_sensitive(true);
                self.show_all(statuses);
            }
            Update::Changed(status) => self.show_one(status),
            Update::Waiting(reason) => {
                *self.daemon.borrow_mut() = None;
                self.refresh.set_sensitive(false);
                self.providers.set_sensitive(false);
                self.show_message("network-offline-symbolic", "Waiting for Tidemark", &reason);
            }
        }
    }

    /// Replaces everything on screen with what the daemon just said it knows.
    fn show_all(&self, statuses: Vec<ProviderStatus>) {
        if statuses.is_empty() {
            // Connected, and there is genuinely nothing to draw. Distinguished from a
            // missing daemon on purpose: one of them is fixed by configuring an account
            // and the other by starting a service.
            self.show_message(
                "view-grid-symbolic",
                "No providers yet",
                "Tidemark is running, but no account is configured.",
            );
            self.cards.borrow_mut().clear();
            self.grid.remove_all();
            return;
        }

        let now = Timestamp::now();
        let cards: Vec<Rc<Card>> = statuses
            .iter()
            .map(|status| Rc::new(Card::new(status, now)))
            .collect();

        self.grid.remove_all();
        *self.cards.borrow_mut() = cards.clone();
        for card in &cards {
            self.grid.append(card.widget());
        }
        self.grid.invalidate_sort();
        self.stack.set_visible_child_name(PAGE_GRID);
        self.update_credentials();
    }

    /// Applies one account's update, adding a card for an account seen for the first time.
    fn show_one(&self, status: ProviderStatus) {
        let now = Timestamp::now();
        let existing = self.cards.borrow().iter().position(|card| {
            let held = card.status();
            held.provider == status.provider && held.account == status.account
        });

        match existing {
            Some(index) => {
                let card = Rc::clone(&self.cards.borrow()[index]);
                card.apply(&status, now);
            }
            None => {
                let card = Rc::new(Card::new(&status, now));
                self.cards.borrow_mut().push(Rc::clone(&card));
                self.grid.append(card.widget());
            }
        }

        // The order may have changed with the numbers; the grid re-sorts what it already
        // holds rather than being rebuilt.
        self.grid.invalidate_sort();
        self.stack.set_visible_child_name(PAGE_GRID);
        self.update_credentials();
    }

    /// Everything the daemon has said, in the order the cards were built.
    fn statuses(&self) -> Vec<ProviderStatus> {
        self.cards
            .borrow()
            .iter()
            .map(|card| card.status())
            .collect()
    }

    /// Feeds the open credentials dialog, and lets go of one the user has closed.
    fn update_credentials(&self) {
        let mut held = self.credentials.borrow_mut();
        match held.as_ref() {
            Some(dialog) if dialog.is_open() => dialog.apply(&self.statuses()),
            Some(_) => *held = None,
            None => {}
        }
    }

    fn connect_providers_button(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        self.providers.connect_clicked(move |_| {
            let Some(main) = weak.upgrade() else {
                return;
            };
            // Already open: present it again rather than stacking a second copy on top of
            // a key somebody is halfway through typing.
            if let Some(dialog) = main.credentials.borrow().as_ref()
                && dialog.is_open()
            {
                return;
            }
            let Some(proxy) = main.daemon.borrow().clone() else {
                return;
            };
            let dialog = Credentials::present(&main.window, proxy, &main.statuses());
            *main.credentials.borrow_mut() = Some(dialog);
        });
    }

    /// Teaches the grid the order the cards go in, once.
    ///
    /// Sorting rather than re-inserting: `GtkFlowBox` keeps its children where they are and
    /// only changes their positions, so a card that overtakes another does not lose focus,
    /// its tooltip, or the pointer that happens to be over it. Rebuilding the grid instead
    /// means re-parenting widgets a disposed wrapper still holds, which GTK reports as a
    /// critical and which costs a redraw of everything on every poll.
    fn install_sort(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        self.grid.set_sort_func(move |left, right| {
            let Some(main) = weak.upgrade() else {
                return Ordering::Equal.into();
            };
            let cards = main.cards.borrow();
            let of = |child: &gtk::FlowBoxChild| cards.iter().find(|card| card.widget() == child);
            match (of(left), of(right)) {
                (Some(left), Some(right)) => model::compare(&left.status(), &right.status()).into(),
                _ => Ordering::Equal.into(),
            }
        });
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
            let Some(main) = weak.upgrade() else {
                return;
            };
            let Some(proxy) = main.daemon.borrow().clone() else {
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
        });
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
