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

use tidemark_types::{AuthCandidate, AuthCandidateState, AuthMode, AuthSelection, ids};

use super::model;

/// Whether activating this candidate may ask the daemon to select it.
///
/// A proven source is a real offer, and a challenged one is nearly that: its jar already
/// holds a live session, and it is the edge refusing the proof rather than the provider
/// refusing the credential — the choice starts working when the challenge lifts. Missing
/// and rejected ones are insensitive — choosing them could only record a failure — and so
/// are locked-keyring and inconclusive verdicts, which are answers about *checking* rather
/// than about the credential.
pub fn candidate_selectable(state: AuthCandidateState) -> bool {
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
pub fn shows_profile_children(children: &[AuthCandidate]) -> bool {
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
pub fn state_word(state: Option<AuthCandidateState>) -> &'static str {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Working,
    Broken,
}

pub fn state_tone(state: Option<AuthCandidateState>) -> Tone {
    match state {
        Some(AuthCandidateState::Ready) => Tone::Working,
        Some(AuthCandidateState::Missing) | Some(AuthCandidateState::Rejected) => Tone::Broken,
        _ => Tone::Plain,
    }
}

/// One line of the half on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub state: Option<AuthCandidateState>,
    pub selectable: bool,
    pub in_use: bool,
    pub indent: bool,
    /// A note standing in for the sources: checking, or nothing offered.
    pub note: bool,
}

impl Line {
    fn note(title: &str) -> Self {
        Self {
            id: String::new(),
            title: title.to_owned(),
            subtitle: String::new(),
            state: None,
            selectable: false,
            in_use: false,
            indent: false,
            note: true,
        }
    }

    fn candidate(candidate: &AuthCandidate, indent: bool) -> Self {
        let state = candidate.state();
        Self {
            id: candidate.id.clone(),
            title: candidate.title.clone(),
            subtitle: candidate.subtitle.clone().unwrap_or_default(),
            state,
            selectable: state.is_some_and(candidate_selectable),
            in_use: false,
            indent,
            note: false,
        }
    }
}

/// One account's local-source page: which half is showing, and what was last inspected.
///
/// The active tab is navigation, not a claim: it moves when clicked and stays where the
/// user leaves it regardless of polls arriving underneath. It follows the published
/// selection only until the first click, which is the last moment the view belongs to the
/// account rather than to the person looking at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserAuth {
    modes: Vec<AuthMode>,
    mode: String,
    navigated: bool,
    selection: Option<AuthSelection>,
    report: Vec<AuthCandidate>,
    /// An inspection is in flight and nothing has been inspected that could stand in.
    checking: bool,
}

impl BrowserAuth {
    /// Laid out on the account's published selection; checking from the start, because
    /// opening the page is what inspects.
    pub fn new(modes: &[AuthMode], selection: Option<&AuthSelection>) -> Self {
        Self {
            modes: modes.to_vec(),
            mode: desired_mode(modes, selection),
            navigated: false,
            selection: selection.cloned(),
            report: Vec::new(),
            checking: true,
        }
    }

    pub fn titles(&self) -> Vec<String> {
        self.modes.iter().map(|mode| mode.title.clone()).collect()
    }

    pub fn mode_index(&self) -> usize {
        self.modes
            .iter()
            .position(|mode| mode.value == self.mode)
            .unwrap_or(0)
    }

    /// Whether the half on screen is the one a session is pasted into.
    pub fn pasting(&self) -> bool {
        self.mode == ids::PASTE_AUTH_MODE
    }

    #[cfg(test)]
    pub fn mode(&self) -> &str {
        &self.mode
    }

    /// A tab click. Once somebody has navigated, their click owns the view for as long as
    /// the dialog is open.
    pub fn choose_mode(&mut self, index: usize) {
        if let Some(mode) = self.modes.get(index) {
            self.navigated = true;
            self.mode = mode.value.clone();
        }
    }

    pub fn begin_checking(&mut self) {
        self.checking = true;
    }

    /// A fresh inspection, and an untouched view settling onto it.
    pub fn apply_report(&mut self, report: Vec<AuthCandidate>) {
        self.report = report;
        self.checking = false;
        self.follow_if_untouched();
    }

    /// A check that never answered. With nothing ever inspected there is no previous
    /// answer to go back to, and the checking note stays up instead.
    pub fn recover(&mut self) {
        self.checking = self.report.is_empty();
    }

    /// What the daemon last published, and an untouched view moving with it.
    pub fn apply_selection(&mut self, selection: Option<&AuthSelection>) {
        self.selection = selection.cloned();
        self.follow_if_untouched();
    }

    fn follow_if_untouched(&mut self) {
        if !self.navigated {
            self.mode = desired_mode(&self.modes, self.selection.as_ref());
        }
    }

    /// The lines of the half on screen.
    ///
    /// The body matching the mode carries the rows. One with no children stands for
    /// itself — activating it claims the whole mode — while one with children lists them
    /// and never claims itself from behind their backs. A body the report stopped sending
    /// leaves the half honest rather than drawn from remembered candidates.
    pub fn lines(&self) -> Vec<Line> {
        if self.checking {
            return vec![Line::note("Checking local sources…")];
        }
        let Some(body) = self.report.iter().find(|body| body.id == self.mode) else {
            return vec![Line::note("Not offered right now")];
        };
        let mut lines = Vec::new();
        if body.children.is_empty() {
            lines.push(Line::candidate(body, false));
        } else {
            for source in &body.children {
                lines.push(Line::candidate(source, false));
                if shows_profile_children(&source.children) {
                    lines.extend(
                        source
                            .children
                            .iter()
                            .map(|profile| Line::candidate(profile, true)),
                    );
                }
            }
        }
        self.mark_in_use(&mut lines);
        lines
    }

    /// Marks the one line the published selection names, inside this half's mode.
    ///
    /// The claim names a profile leaf, while the lines stop at the browser whenever its
    /// profiles are not drawn — the line that carries the claim is the longest id the
    /// claim is or lives under.
    fn mark_in_use(&self, lines: &mut [Line]) {
        let Some(claimed) = self
            .selection
            .as_ref()
            .and_then(|selection| model::selected_candidate(selection, &self.mode))
        else {
            return;
        };
        let marked = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                !line.note
                    && (claimed == line.id
                        || claimed
                            .strip_prefix(line.id.as_str())
                            .is_some_and(|rest| rest.starts_with('/')))
            })
            .max_by_key(|(_, line)| line.id.len())
            .map(|(index, _)| index);
        if let Some(index) = marked {
            lines[index].in_use = true;
        }
    }

    /// The selection a click on this line asks for. A self-representing mode is claimed by
    /// claiming the mode itself; everything underneath rides on naming its own id.
    pub fn selection_for(&self, id: &str) -> AuthSelection {
        AuthSelection {
            mode: self.mode.clone(),
            candidate: (id != self.mode).then(|| id.to_owned()),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, state: AuthCandidateState) -> AuthCandidate {
        AuthCandidate {
            id: id.into(),
            title: id.into(),
            subtitle: None,
            state: state.as_wire().into(),
            children: Vec::new(),
        }
    }

    fn mode(value: &str) -> AuthMode {
        AuthMode {
            value: value.into(),
            title: value.to_uppercase(),
        }
    }

    fn browser_report() -> Vec<AuthCandidate> {
        vec![AuthCandidate {
            children: vec![candidate("firefox", AuthCandidateState::Ready)],
            ..candidate("browser", AuthCandidateState::Ready)
        }]
    }

    #[test]
    fn a_proven_or_challenged_candidate_is_selectable() {
        assert!(candidate_selectable(AuthCandidateState::Ready));
        assert!(candidate_selectable(AuthCandidateState::Challenged));
    }

    #[test]
    fn a_source_without_a_usable_credential_cannot_be_chosen() {
        assert!(!candidate_selectable(AuthCandidateState::Missing));
        assert!(!candidate_selectable(AuthCandidateState::Rejected));
        assert!(!candidate_selectable(AuthCandidateState::WaitingForKeyring));
        assert!(!candidate_selectable(AuthCandidateState::Unreachable));
    }

    #[test]
    fn locked_keyrings_and_unanswered_checks_are_never_red() {
        assert_eq!(
            state_tone(Some(AuthCandidateState::WaitingForKeyring)),
            Tone::Plain
        );
        assert_eq!(
            state_tone(Some(AuthCandidateState::Unreachable)),
            Tone::Plain
        );
        assert_eq!(state_tone(Some(AuthCandidateState::Ready)), Tone::Working);
        assert_eq!(state_tone(Some(AuthCandidateState::Rejected)), Tone::Broken);
        assert_eq!(state_tone(Some(AuthCandidateState::Missing)), Tone::Broken);
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
            state_word(Some(AuthCandidateState::Challenged)),
            "Browser check"
        );
        // A verdict this build does not know draws the neutral words instead of guessing
        // that a source a newer daemon doubts is usable.
        assert_eq!(state_word(None), "Inconclusive");
        assert_eq!(state_tone(None), Tone::Plain);
    }

    #[test]
    fn profiles_surface_only_when_more_than_one_of_them_are_ready() {
        let two_ready = vec![
            candidate("zen/A", AuthCandidateState::Ready),
            candidate("zen/B", AuthCandidateState::Ready),
        ];
        assert!(shows_profile_children(&two_ready));
        let one_ready = vec![
            candidate("zen/A", AuthCandidateState::Ready),
            candidate("zen/B", AuthCandidateState::Missing),
        ];
        assert!(!shows_profile_children(&one_ready));
    }

    #[test]
    fn the_page_checks_until_the_first_report_and_keeps_a_report_through_a_failure() {
        let mut auth = BrowserAuth::new(&[mode("browser")], None);
        assert!(auth.lines()[0].note);
        auth.recover();
        assert_eq!(auth.lines()[0].title, "Checking local sources…");

        auth.apply_report(browser_report());
        auth.begin_checking();
        auth.recover();
        assert_eq!(auth.lines()[0].id, "firefox");
    }

    #[test]
    fn the_in_use_mark_survives_a_new_report_and_lands_on_the_drawn_ancestor() {
        let selection = AuthSelection {
            mode: "browser".into(),
            candidate: Some("firefox".into()),
        };
        let mut auth = BrowserAuth::new(&[mode("browser")], Some(&selection));
        auth.apply_report(browser_report());
        auth.apply_report(browser_report());
        assert!(auth.lines()[0].in_use);

        // The claim names a profile leaf while the rows stop at the browser: the browser
        // row carries it.
        let leaf = AuthSelection {
            mode: "browser".into(),
            candidate: Some("firefox/Default".into()),
        };
        auth.apply_selection(Some(&leaf));
        assert!(auth.lines()[0].in_use);
    }

    #[test]
    fn an_untouched_view_follows_the_published_mode_and_a_clicked_one_stays() {
        let modes = [mode("cursor-app"), mode("browser")];
        let mut auth = BrowserAuth::new(&modes, None);
        assert_eq!(auth.mode(), "cursor-app");

        let browser = AuthSelection {
            mode: "browser".into(),
            candidate: None,
        };
        auth.apply_selection(Some(&browser));
        assert_eq!(auth.mode(), "browser");

        auth.choose_mode(0);
        auth.apply_selection(Some(&browser));
        assert_eq!(auth.mode(), "cursor-app");
    }

    #[test]
    fn a_self_representing_mode_is_claimed_by_naming_the_mode() {
        let auth = BrowserAuth::new(&[mode("cursor-app")], None);
        assert_eq!(auth.selection_for("cursor-app").candidate, None);
        assert_eq!(
            auth.selection_for("zen/A").candidate.as_deref(),
            Some("zen/A")
        );
    }

    #[test]
    fn a_mode_the_report_stopped_sending_is_said_so() {
        let mut auth = BrowserAuth::new(&[mode("paste")], None);
        auth.apply_report(browser_report());
        assert_eq!(auth.lines()[0].title, "Not offered right now");
        assert!(auth.pasting());
    }
}
