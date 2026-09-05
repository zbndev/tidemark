//! The Authentication halves for a provider whose login comes from one explicitly chosen
//! local source.
//!
//! Everything here is driven by what the daemon publishes: the tabs are the selector's
//! own mode titles, the rows are an inspected report's candidates, and activating one asks
//! the daemon to validate and store that choice. Nothing matches Cursor or any browser by
//! name, reads a file, or opens a socket — that is daemon work this crate only requests.
//!
//! Tabs and rows play different parts. A tab only swaps which half is on screen: some
//! modes need a candidate picked from their half before anything can be stored at all, so
//! a tab alone is never a complete answer. A row activation is the whole selection.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use tidemark_types::{
    AuthCandidate, AuthCandidateState, AuthMode, AuthSelection, AuthSelector, ids,
};

use super::model;

/// Whether activating this candidate may ask the daemon to select it.
///
/// A proven source is a real offer, and a challenged one is nearly that: its jar already
/// holds a live session, and it is the edge refusing the proof rather than the provider
/// refusing the credential — the choice starts working when the challenge lifts. Missing
/// and rejected ones are insensitive — choosing them could only record a failure — and so
/// are locked-keyring and inconclusive verdicts, which are answers about *checking* rather
/// than about the credential.
pub(super) fn candidate_selectable(state: AuthCandidateState) -> bool {
    matches!(
        state,
        AuthCandidateState::Ready | AuthCandidateState::Challenged
    )
}

/// Whether a source's children deserve rows of their own.
///
/// One ready child needs no second question: activating the parent picks it, in the
/// stable scan order the daemon resolves with. Only when more than one works does the
/// rare second choice surface, and then only the working ones.
pub(super) fn shows_profile_children(children: &[AuthCandidate]) -> bool {
    children
        .iter()
        .filter(|child| child.state().is_some_and(candidate_selectable))
        .count()
        > 1
}

/// The words for a candidate's verdict, whatever the colour says beside them.
///
/// A verdict this build does not know counts as inconclusive rather than usable or
/// broken: a newer daemon invented it, and guessing its meaning here would outvote the
/// daemon.
pub(super) fn state_word(state: Option<AuthCandidateState>) -> &'static str {
    match state {
        Some(AuthCandidateState::Ready) => "Working",
        Some(AuthCandidateState::Missing) => "Not found",
        Some(AuthCandidateState::Rejected) => "Rejected",
        Some(AuthCandidateState::WaitingForKeyring) => "Keyring locked",
        Some(AuthCandidateState::Challenged) => "Browser check",
        Some(AuthCandidateState::Unreachable) | None => "Inconclusive",
    }
}

/// How a verdict is coloured: green for proven, red for disproven, plain words otherwise.
pub(super) fn state_classes(state: Option<AuthCandidateState>) -> &'static [&'static str] {
    match state {
        Some(AuthCandidateState::Ready) => &["success"],
        Some(AuthCandidateState::Missing) | Some(AuthCandidateState::Rejected) => &["error"],
        _ => &["dim-label"],
    }
}

/// One selectable source line, and whether the account currently runs on it.
#[derive(Debug)]
struct CandidateRow {
    id: String,
    row: adw::ActionRow,
    in_use: gtk::Image,
}

impl CandidateRow {
    /// Activates through `on_choose`, which the detail page turns into a validated write;
    /// nothing moves on click, because the claim becomes real only where the daemon says.
    fn new(
        id: String,
        title: &str,
        subtitle: Option<&str>,
        state: Option<AuthCandidateState>,
        selectable: bool,
        mode_value: &str,
        on_choose: Rc<dyn Fn(AuthSelection)>,
    ) -> Rc<Self> {
        // Markup off twice over: titles and subtitles are the daemon's words, which are
        // data, never markup.
        let row = adw::ActionRow::builder()
            .title(title)
            .use_markup(false)
            .sensitive(selectable)
            .activatable(selectable)
            .build();
        if let Some(subtitle) = subtitle {
            row.set_subtitle(subtitle);
        }

        let word = gtk::Label::builder()
            .label(state_word(state))
            .valign(gtk::Align::Center)
            .css_classes(state_classes(state))
            .build();
        row.add_suffix(&word);
        row.update_property(&[gtk::accessible::Property::Description(&format!(
            "{title}: {}",
            state_word(state)
        ))]);

        let in_use = gtk::Image::builder()
            .icon_name("object-select-symbolic")
            .tooltip_text("In use")
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        row.add_suffix(&in_use);

        if selectable {
            let claimed_here = id == mode_value;
            let mode_claimed = mode_value.to_owned();
            let identifier = id.clone();
            row.connect_activated(move |_| {
                // A self-representing mode is claimed by claiming the mode itself;
                // everything underneath rides on naming its own identifier.
                on_choose(AuthSelection {
                    mode: mode_claimed.clone(),
                    candidate: (!claimed_here).then(|| identifier.clone()),
                });
            });
        }

        Rc::new(Self { id, row, in_use })
    }
}

/// One half of the choice: every source behind one selector mode, plus what an inspection
/// in flight has to say meanwhile.
///
/// Rows are rebuilt together whenever an inspection lands, and left alone until then,
/// because replacing a row under a pointer that is on it loses the click. The whole half
/// hides instead of rebuilding when another tab is showing.
#[derive(Debug)]
struct Half {
    mode_value: String,
    group: adw::PreferencesGroup,
    notes: RefCell<Vec<adw::ActionRow>>,
    rows: RefCell<Vec<Rc<CandidateRow>>>,
    /// The place to paste a session, on the one mode that has one. Held apart from the
    /// rows because it is not built from a report: a report landing while somebody is
    /// half-way through pasting a header must not take the header away.
    entry: Option<adw::PasswordEntryRow>,
}

impl Half {
    fn new(mode: &AuthMode, on_paste: &Rc<dyn Fn(String)>) -> Self {
        let group = adw::PreferencesGroup::builder()
            .visible(false)
            .margin_top(12)
            .build();
        // The one mode whose source is the person rather than something to be discovered:
        // its half is where a header is put in, and the row beneath says what the daemon
        // made of the last one.
        let entry = (mode.value == ids::PASTE_AUTH_MODE).then(|| paste_entry(&group, on_paste));

        Self {
            mode_value: mode.value.clone(),
            group,
            notes: RefCell::new(Vec::new()),
            rows: RefCell::new(Vec::new()),
            entry,
        }
    }

    fn set_visible(&self, visible: bool) {
        self.group.set_visible(visible);
        // Navigating away drops a half-typed header rather than leaving a live session
        // sitting in a hidden field for the rest of the dialog's life. Only reachable
        // after a click on this half's tab, which is what stops a report arriving
        // underneath from clearing what somebody is in the middle of typing.
        if !visible && let Some(entry) = &self.entry {
            entry.set_text("");
        }
    }

    fn clear_contents(&self) {
        for note in self.notes.borrow_mut().drain(..) {
            self.group.remove(&note);
        }
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row.row);
        }
    }

    /// Draws the checking note. Whatever report was shown before stays recorded, so a
    /// failed inspection puts those same rows back rather than an empty half.
    fn show_checking(&self) {
        self.clear_contents();
        let note = adw::ActionRow::builder()
            .title("Checking local sources…")
            .sensitive(false)
            .use_markup(false)
            .build();
        self.group.add(&note);
        self.notes.borrow_mut().push(note);
    }

    /// Draws one inspected report: every source of this half's mode.
    ///
    /// The body matching the mode carries the rows. One with no children stands for
    /// itself — activating it claims the whole mode — while one with children lists them
    /// and never claims itself from behind their backs. A body the report stopped sending
    /// leaves the half honest rather than drawn from remembered candidates.
    fn apply_report(&self, report: &[AuthCandidate], on_choose: &Rc<dyn Fn(AuthSelection)>) {
        self.clear_contents();
        let Some(body) = report.iter().find(|body| body.id == self.mode_value) else {
            let note = adw::ActionRow::builder()
                .title("Not offered right now")
                .sensitive(false)
                .use_markup(false)
                .build();
            self.group.add(&note);
            self.notes.borrow_mut().push(note);
            return;
        };

        if body.children.is_empty() {
            self.add_candidate(body, 0, on_choose);
            return;
        }
        for source in &body.children {
            self.add_candidate(source, 0, on_choose);
            if shows_profile_children(&source.children) {
                for profile in &source.children {
                    self.add_candidate(profile, 18, on_choose);
                }
            }
        }
    }

    fn add_candidate(
        &self,
        candidate: &AuthCandidate,
        indent: i32,
        on_choose: &Rc<dyn Fn(AuthSelection)>,
    ) {
        let state = candidate.state();
        let row = CandidateRow::new(
            candidate.id.clone(),
            &candidate.title,
            candidate.subtitle.as_deref(),
            state,
            state.is_some_and(candidate_selectable),
            &self.mode_value,
            Rc::clone(on_choose),
        );
        row.row.set_margin_start(indent);
        self.group.add(&row.row);
        self.rows.borrow_mut().push(row);
    }

    /// Marks the one row the account's published selection names, inside its own mode.
    ///
    /// The claim names a profile leaf, while the rows stop at the browser whenever its
    /// profiles are not drawn — the row that carries the claim is the longest row id the
    /// claim is or lives under.
    fn apply_selection(&self, selection: Option<&AuthSelection>) {
        let claimed =
            selection.and_then(|selection| model::selected_candidate(selection, &self.mode_value));
        let rows = self.rows.borrow();
        let marked = claimed.and_then(|claimed| {
            rows.iter()
                .filter(|row| {
                    claimed == row.id
                        || claimed
                            .strip_prefix(row.id.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                })
                .max_by_key(|row| row.id.len())
                .map(|row| row.id.clone())
        });
        for row in rows.iter() {
            row.in_use
                .set_visible(marked.as_deref() == Some(row.id.as_str()));
        }
    }
}

/// The tab pill and every half beneath it, built once and refreshed from inspections.
///
/// The active tab is navigation, not a claim: it moves when clicked and stays where the
/// user leaves it regardless of polls arriving underneath. It follows the published
/// selection only until the first click, which is the last moment the view belongs to the
/// account rather than to the person looking at it.
pub(super) struct BrowserAuth {
    pill_row: adw::PreferencesRow,
    toggles: adw::ToggleGroup,
    modes: Vec<AuthMode>,
    halves: Vec<Half>,
    on_choose: Rc<dyn Fn(AuthSelection)>,
    suppress_toggle: Rc<Cell<bool>>,
    navigated: Rc<Cell<bool>>,
    selection: RefCell<Option<AuthSelection>>,
    report: RefCell<Vec<AuthCandidate>>,
}

// No closure or stored callback belongs in a Debug rendering: they are wiring, not state.
impl std::fmt::Debug for BrowserAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BrowserAuth")
            .field("toggles", &self.toggles)
            .field("modes", &self.modes)
            .field("halves", &self.halves)
            .field("navigated", &self.navigated.get())
            .field("selection", &self.selection)
            .finish_non_exhaustive()
    }
}

impl BrowserAuth {
    /// Builds the pill and one half per mode, laid out on the account's published
    /// selection before anything can be clicked.
    ///
    /// Checking starts here already: until [`Self::apply_report`] replaces them, each
    /// half draws the note a machine deserves while its sources are being looked at.
    pub(super) fn new(
        selector: &AuthSelector,
        status_selection: Option<&AuthSelection>,
        on_choose: Rc<dyn Fn(AuthSelection)>,
        on_paste: Rc<dyn Fn(String)>,
    ) -> Self {
        // Full width, in a row of its own at the top of the group, rather than tucked
        // into the group's header — the same shape the OAuth/CLI credential pill takes,
        // because both carry the provider's own names for two alternative logins.
        let toggles = adw::ToggleGroup::builder()
            .hexpand(true)
            .homogeneous(true)
            .build();
        for mode in &selector.modes {
            toggles.add(
                adw::Toggle::builder()
                    .name(&mode.value)
                    .label(&mode.title)
                    .build(),
            );
        }
        let pill_row = adw::PreferencesRow::builder()
            .activatable(false)
            .child(&toggles)
            .build();
        pill_row.add_css_class("credential-choice");

        let halves: Vec<Half> = selector
            .modes
            .iter()
            .map(|mode| Half::new(mode, &on_paste))
            .collect();

        let suppress_toggle = Rc::new(Cell::new(false));
        let navigated = Rc::new(Cell::new(false));

        let initial = desired_mode(&selector.modes, status_selection);
        toggles.set_active_name(Some(initial.as_str()));
        position_halves(&halves, &initial);

        let clicked = toggles.clone();
        toggles.connect_active_name_notify({
            let suppress_toggle = Rc::clone(&suppress_toggle);
            let navigated = Rc::clone(&navigated);
            let shown: Vec<(String, adw::PreferencesGroup)> = halves
                .iter()
                .map(|half| (half.mode_value.clone(), half.group.clone()))
                .collect();
            move |_| {
                if suppress_toggle.get() {
                    return;
                }
                navigated.set(true);
                if let Some(name) = active_name(&clicked) {
                    for (mode_value, group) in &shown {
                        group.set_visible(mode_value == &name);
                    }
                }
            }
        });

        let built = Self {
            pill_row,
            toggles,
            modes: selector.modes.clone(),
            halves,
            on_choose,
            suppress_toggle,
            navigated,
            selection: RefCell::new(status_selection.cloned()),
            report: RefCell::new(Vec::new()),
        };
        built.begin_loading();
        built
    }

    /// Adds everything this capability draws to the Authentication group.
    pub(super) fn attach(&self, authentication: &adw::PreferencesGroup) {
        authentication.add(&self.pill_row);
        for half in &self.halves {
            authentication.add(&half.group);
        }
    }

    /// Puts every half into its checking state ahead of an inspection. A report still
    /// recorded underneath survives the note, so a failed refresh restores those rows.
    pub(super) fn begin_loading(&self) {
        for half in &self.halves {
            half.show_checking();
        }
    }

    /// Undoes a check that never answered. With nothing ever inspected there is no
    /// previous answer to go back to, and the checking note stays up instead.
    pub(super) fn recover(&self) {
        if self.report.borrow().is_empty() {
            self.begin_loading();
            return;
        }
        let kept = self.report.borrow().clone();
        self.render_report(&kept);
    }

    /// Renders a fresh inspection, and lets an untouched view settle onto it.
    pub(super) fn apply_report(&self, report: Vec<AuthCandidate>) {
        *self.report.borrow_mut() = report;
        let kept = self.report.borrow().clone();
        self.render_report(&kept);
        self.follow_if_untouched();
    }

    /// Repaints the In-use marks from what the daemon last published, and moves an
    /// untouched view along with it.
    pub(super) fn apply_selection(&self, selection: Option<&AuthSelection>) {
        *self.selection.borrow_mut() = selection.cloned();
        for half in &self.halves {
            half.apply_selection(selection);
        }
        self.follow_if_untouched();
    }

    fn render_report(&self, report: &[AuthCandidate]) {
        for half in &self.halves {
            half.apply_report(report, &self.on_choose);
        }
        // Rebuilt rows start unmarked: repaint the claim the daemon last published rather
        // than leaving it to the next unrelated status publication.
        let selection = self.selection.borrow().clone();
        for half in &self.halves {
            half.apply_selection(selection.as_ref());
        }
    }

    /// Snaps the pill and the halves onto the published mode while nobody has navigated.
    /// Once somebody has, their click owns the view for the life of the dialog, whatever
    /// lands in the queue afterwards.
    fn follow_if_untouched(&self) {
        if self.navigated.get() {
            return;
        }
        let wanted = {
            let selection = self.selection.borrow();
            desired_mode(&self.modes, selection.as_ref())
        };
        self.set_active_silently(&wanted);
        position_halves(&self.halves, &wanted);
    }

    fn set_active_silently(&self, mode_value: &str) {
        self.suppress_toggle.set(true);
        self.toggles.set_active_name(Some(mode_value));
        self.suppress_toggle.set(false);
    }
}

/// The place a session is pasted in, added to the paste half and kept for its lifetime.
///
/// The header is cleared out of the widget the moment it is handed over: it is a live
/// session, and leaving it in an entry would leave it on screen behind a reveal button
/// for as long as the dialog stays open.
fn paste_entry(
    group: &adw::PreferencesGroup,
    on_paste: &Rc<dyn Fn(String)>,
) -> adw::PasswordEntryRow {
    let entry = adw::PasswordEntryRow::builder()
        .title("Cookie header")
        .build();
    let save = gtk::Button::builder()
        .label("Save")
        .valign(gtk::Align::Center)
        .sensitive(false)
        .css_classes(["suggested-action"])
        .build();
    entry.add_suffix(&save);
    entry.connect_changed({
        let save = save.clone();
        move |entry| save.set_sensitive(!entry.text().trim().is_empty())
    });

    let store = {
        let entry = entry.clone();
        let on_paste = Rc::clone(on_paste);
        move || {
            let pasted = entry.text().trim().to_owned();
            if pasted.is_empty() {
                return;
            }
            entry.set_text("");
            on_paste(pasted);
        }
    };
    save.connect_clicked({
        let store = store.clone();
        move |_| store()
    });
    entry.connect_entry_activated(move |_| store());
    group.add(&entry);
    entry
}

fn active_name(toggles: &adw::ToggleGroup) -> Option<String> {
    toggles.active_name().map(|name| name.to_string())
}

/// The mode an open view should start on, or stay on while untouched.
///
/// The published selection wins when the daemon named a mode this build knows; the first
/// declared mode covers an account whose source was never picked, and both fall back
/// harmlessly when a selector has no modes to show at all.
fn desired_mode(modes: &[AuthMode], selection: Option<&AuthSelection>) -> String {
    match selection {
        Some(selection) if modes.iter().any(|mode| mode.value == selection.mode) => {
            selection.mode.clone()
        }
        _ => modes
            .first()
            .map(|mode| mode.value.clone())
            .unwrap_or_default(),
    }
}

fn position_halves(halves: &[Half], visible_mode: &str) {
    for half in halves {
        half.set_visible(half.mode_value == visible_mode);
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use adw::prelude::*;
    use tidemark_types::{
        AuthCandidate, AuthCandidateState, AuthMode, AuthSelection, AuthSelector,
    };

    use super::{
        BrowserAuth, CandidateRow, Half, candidate_selectable, shows_profile_children,
        state_classes, state_word,
    };

    fn candidate(id: &str, state: AuthCandidateState) -> AuthCandidate {
        AuthCandidate {
            id: id.into(),
            title: id.into(),
            subtitle: None,
            state: state.as_wire().into(),
            children: Vec::new(),
        }
    }

    #[test]
    fn a_proven_candidate_is_selectable() {
        assert!(candidate_selectable(AuthCandidateState::Ready));
    }

    /// GTK widget state needs a display, while the rest of this module's tests intentionally
    /// stay headless. Keep all such assertions together: GTK binds initialization to the
    /// harness thread.
    fn widgets() -> bool {
        static READY: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| adw::init().is_ok());
        *READY
    }

    #[test]
    fn only_the_paste_half_takes_a_header_and_it_keeps_none_of_it_behind() {
        if !widgets() {
            eprintln!("skipped: no display is available");
            return;
        }

        let on_paste: Rc<dyn Fn(String)> = Rc::new(|_| {});
        let paste = Half::new(
            &AuthMode {
                value: "paste".into(),
                title: "Paste session".into(),
            },
            &on_paste,
        );
        let browser = Half::new(
            &AuthMode {
                value: "browser".into(),
                title: "Browser".into(),
            },
            &on_paste,
        );

        let entry = paste
            .entry
            .clone()
            .expect("the paste half is somewhere to paste");
        assert!(
            browser.entry.is_none(),
            "a half that lists discovered sources has nothing to type into"
        );

        // A live session left in a hidden field would stay on screen, behind one reveal
        // button, for as long as the dialog is open.
        entry.set_text("session=tok");
        paste.set_visible(false);
        assert_eq!(entry.text(), "");
    }

    #[test]
    fn browser_auth_widgets_keep_rows_separate_and_marks_across_rebuilds() {
        if !widgets() {
            eprintln!("skipped: no display is available");
            return;
        }

        let row = CandidateRow::new(
            "firefox".into(),
            "Firefox",
            None,
            Some(AuthCandidateState::Ready),
            true,
            "browser",
            Rc::new(|_| {}),
        );

        assert!(row.row.is_activatable());

        let half = Half::new(
            &AuthMode {
                value: "browser".into(),
                title: "Browser".into(),
            },
            &(Rc::new(|_| {}) as Rc<dyn Fn(String)>),
        );

        assert!(half.group.title().is_empty());
        assert!(half.group.header_suffix().is_none());
        assert_eq!(half.group.margin_top(), 12);

        // Inspection rebuilds every row; the In-use mark must not wait for the next
        // unrelated status publication to come back.
        let selector = AuthSelector {
            option: "auth-source".into(),
            modes: vec![AuthMode {
                value: "browser".into(),
                title: "Browser".into(),
            }],
        };
        let selection = AuthSelection {
            mode: "browser".into(),
            candidate: Some("firefox".into()),
        };
        let auth = BrowserAuth::new(
            &selector,
            Some(&selection),
            Rc::new(|_| {}),
            Rc::new(|_| {}),
        );
        let report = || {
            vec![AuthCandidate {
                children: vec![candidate("firefox", AuthCandidateState::Ready)],
                ..candidate("browser", AuthCandidateState::Ready)
            }]
        };

        auth.apply_report(report());
        auth.apply_report(report());

        let marks = |auth: &BrowserAuth| -> Vec<bool> {
            auth.halves[0]
                .rows
                .borrow()
                .iter()
                .map(|row| row.in_use.is_visible())
                .collect()
        };
        assert_eq!(marks(&auth), [true]);

        // The claim names a profile leaf while the rows stop at the browser: the browser
        // row carries it.
        let leaf = AuthSelection {
            mode: "browser".into(),
            candidate: Some("firefox/Default".into()),
        };
        let auth = BrowserAuth::new(&selector, Some(&leaf), Rc::new(|_| {}), Rc::new(|_| {}));
        auth.apply_report(report());
        auth.apply_report(report());
        assert_eq!(marks(&auth), [true]);
    }

    #[test]
    fn a_source_without_a_usable_credential_cannot_be_chosen() {
        assert!(!candidate_selectable(AuthCandidateState::Missing));
        assert!(!candidate_selectable(AuthCandidateState::Rejected));
    }

    #[test]
    fn locked_keyrings_and_unanswered_checks_are_insensitive_but_never_red() {
        assert!(!candidate_selectable(AuthCandidateState::WaitingForKeyring));
        assert!(!candidate_selectable(AuthCandidateState::Unreachable));

        assert_eq!(
            state_classes(Some(AuthCandidateState::WaitingForKeyring)),
            &["dim-label"]
        );
        assert_eq!(
            state_classes(Some(AuthCandidateState::Unreachable)),
            &["dim-label"]
        );
        assert_eq!(state_classes(Some(AuthCandidateState::Ready)), &["success"]);
        assert_eq!(
            state_classes(Some(AuthCandidateState::Rejected)),
            &["error"]
        );
        assert_eq!(state_classes(Some(AuthCandidateState::Missing)), &["error"]);
    }

    #[test]
    fn every_verdict_has_words_so_colour_is_never_the_only_message() {
        assert_eq!(state_word(Some(AuthCandidateState::Ready)), "Working");
        assert_eq!(state_word(Some(AuthCandidateState::Missing)), "Not found");
        assert_eq!(state_word(Some(AuthCandidateState::Rejected)), "Rejected");
        assert_eq!(
            state_word(Some(AuthCandidateState::WaitingForKeyring)),
            "Keyring locked"
        );
        assert_eq!(
            state_word(Some(AuthCandidateState::Unreachable)),
            "Inconclusive"
        );

        // A verdict this build does not know draws the neutral words instead of guessing
        // that a source a newer daemon doubts is usable.
        assert_eq!(state_word(None), "Inconclusive");
        assert_eq!(state_classes(None), &["dim-label"]);
    }

    #[test]
    fn a_challenged_candidate_is_selectable_with_neutral_words() {
        // The jar already holds a live session; it is the edge refusing the proof, not the
        // provider refusing the credential.
        assert!(candidate_selectable(AuthCandidateState::Challenged));
        assert_eq!(
            state_word(Some(AuthCandidateState::Challenged)),
            "Browser check"
        );
        assert_eq!(
            state_classes(Some(AuthCandidateState::Challenged)),
            &["dim-label"]
        );
    }

    #[test]
    fn profiles_surface_only_when_more_than_one_of_them_are_ready() {
        let two_ready = vec![
            candidate("zen/A", AuthCandidateState::Ready),
            candidate("zen/B", AuthCandidateState::Ready),
        ];
        assert!(shows_profile_children(&two_ready));

        // One working profile needs no second choice: activating the browser picks it.
        let one_ready = vec![
            candidate("zen/A", AuthCandidateState::Ready),
            candidate("zen/B", AuthCandidateState::Missing),
        ];
        assert!(!shows_profile_children(&one_ready));

        let nothing_ready = vec![
            candidate("zen/A", AuthCandidateState::Missing),
            candidate("zen/B", AuthCandidateState::Rejected),
        ];
        assert!(!shows_profile_children(&nothing_ready));
    }
}
