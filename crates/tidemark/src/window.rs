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
use tidemark_types::{ProviderDefinition, ProviderStatus, Timestamp};

use crate::bus::{self, DaemonProxy, Update};
use crate::card::Card;
use crate::model;
use crate::provider_settings::ProviderSettings;

/// How often the clock-dependent parts of every card are redrawn. Half a minute is well
/// inside the resolution of everything shown — the coarsest unit on a card is a minute —
/// and costs a redraw of a handful of labels.
const TICK_SECONDS: u32 = 30;

/// Names of the pages in the stack.
const PAGE_GRID: &str = "grid";
const PAGE_MESSAGE: &str = "message";

fn account_index(statuses: &[ProviderStatus], provider: &str, account: &str) -> Option<usize> {
    statuses
        .iter()
        .position(|status| status.provider == provider && status.account == account)
}

#[derive(Debug)]
struct DialogSlot<T>(RefCell<Option<Rc<T>>>);

impl<T> Default for DialogSlot<T> {
    fn default() -> Self {
        Self(RefCell::new(None))
    }
}

impl<T> DialogSlot<T> {
    fn insert_if_empty(&self, value: Rc<T>) -> bool {
        let mut held = self.0.borrow_mut();
        if held.is_some() {
            return false;
        }
        *held = Some(value);
        true
    }

    fn get(&self) -> Option<Rc<T>> {
        self.0.borrow().clone()
    }

    fn is_empty(&self) -> bool {
        self.0.borrow().is_none()
    }

    fn clear(&self) {
        self.0.borrow_mut().take();
    }
}

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
    definitions: RefCell<Vec<ProviderDefinition>>,
    daemon: RefCell<Option<DaemonProxy<'static>>>,
    /// The provider dialog while it is open, so live catalog and status changes reach it.
    provider_settings: DialogSlot<ProviderSettings>,
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
            definitions: RefCell::new(Vec::new()),
            daemon: RefCell::new(None),
            provider_settings: DialogSlot::default(),
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
            Update::Connected(proxy, definitions, statuses) => {
                *self.daemon.borrow_mut() = Some(proxy);
                *self.definitions.borrow_mut() = definitions;
                self.refresh.set_sensitive(true);
                self.providers.set_sensitive(true);
                self.show_all(statuses);
            }
            Update::Changed(status) => self.show_one(status),
            Update::Removed { provider, account } => self.show_removed(&provider, &account),
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
                "Welcome to Tidemark",
                "Add a provider to start tracking your quota.",
            );
            self.cards.borrow_mut().clear();
            self.grid.remove_all();
            self.update_provider_settings();
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
        self.update_provider_settings();
    }

    /// Removes one account's card after the daemon confirms it is no longer configured.
    fn show_removed(&self, provider: &str, account: &str) {
        let index = account_index(&self.statuses(), provider, account);
        let Some(index) = index else {
            return;
        };

        let card = self.cards.borrow_mut().remove(index);
        self.grid.remove(card.widget());
        if self.cards.borrow().is_empty() {
            self.show_message(
                "view-grid-symbolic",
                "Welcome to Tidemark",
                "Add a provider to start tracking your quota.",
            );
        }
        self.update_provider_settings();
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
        self.update_provider_settings();
    }

    /// Everything the daemon has said, in the order the cards were built.
    fn statuses(&self) -> Vec<ProviderStatus> {
        self.cards
            .borrow()
            .iter()
            .map(|card| card.status())
            .collect()
    }

    /// Feeds the open provider dialog without consulting widget visibility for ownership.
    fn update_provider_settings(&self) {
        if let Some(dialog) = self.provider_settings.get() {
            dialog.apply(&self.definitions.borrow(), &self.statuses());
        }
    }

    fn connect_providers_button(self: &Rc<Self>) {
        let weak: Weak<Self> = Rc::downgrade(self);
        self.providers.connect_clicked(move |_| {
            let Some(main) = weak.upgrade() else {
                return;
            };
            if !main.provider_settings.is_empty() {
                return;
            }
            let Some(proxy) = main.daemon.borrow().clone() else {
                return;
            };
            let on_closed = {
                let weak = weak.clone();
                move || {
                    if let Some(main) = weak.upgrade() {
                        main.provider_settings.clear();
                    }
                }
            };
            let dialog = ProviderSettings::present(
                &main.window,
                proxy,
                &main.definitions.borrow(),
                &main.statuses(),
                on_closed,
            );
            assert!(main.provider_settings.insert_if_empty(dialog));
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

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use tidemark_types::{AccountId, ProviderId, ProviderStatus};

    use super::{DialogSlot, account_index};

    fn status(provider: &str, account: &str) -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new(provider), &AccountId::new(account))
    }

    #[test]
    fn removal_matches_the_full_provider_account_identity() {
        let statuses = vec![status("zai", "first"), status("zai", "default")];
        assert_eq!(account_index(&statuses, "zai", "default"), Some(1));
        assert_eq!(account_index(&statuses, "kimi", "default"), None);
    }

    #[test]
    fn a_closed_dialog_slot_can_be_filled_again() {
        let slot = DialogSlot::default();
        assert!(slot.insert_if_empty(Rc::new(1)));
        assert!(!slot.insert_if_empty(Rc::new(2)));
        slot.clear();
        assert!(slot.insert_if_empty(Rc::new(3)));
    }
}
