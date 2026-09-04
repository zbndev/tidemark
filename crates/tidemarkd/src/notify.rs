//! Deciding what to say about a window, and saying it.
//!
//! Two halves that never touch each other. **What is worth saying** is a pure function of
//! one reading and of what has already been said about the segment it landed in — no
//! clock, no bus, no database — which is what makes the awkward cases testable: a window
//! first seen at ninety-six percent, a rollover into a window that is already hot, a
//! restart that must not warn somebody twice.
//!
//! **Saying it** is a platform desktop notification transport, and the daemon holds no
//! opinion about what the user's desktop does with it beyond urgency.
//!
//! The deduplication key is the segment, filed in the history database rather than in
//! memory — see `storage::History::record_notice`. A row is written only after the
//! notification server has accepted the message, so a desktop that is not listening yet
//! gets the warning on the next poll instead of never.

use std::collections::HashMap;
use std::sync::Mutex;
#[cfg(unix)]
use std::time::Duration;

use tidemark_core::providers::BoxFuture;
use tidemark_types::{DANGER_AT, Timestamp, WARNING_AT, Window, present, provider_label};

/// How long the notification server is given to answer.
///
/// It is a local call and normally returns in milliseconds. The bound exists because the
/// poll loop awaits this: a notification daemon wedged mid-restart must cost one warning,
/// not every provider's next reading.
#[cfg(unix)]
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// One of the two levels of consumption worth interrupting somebody about.
///
/// The same two the bar changes colour at, from the same constants: the card and the
/// notification must never disagree about when a window became worth worrying about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Threshold {
    /// [`WARNING_AT`] — the pace is worth a look.
    Warning,
    /// [`DANGER_AT`] — the quota is nearly gone.
    Danger,
}

impl Threshold {
    /// Both, quietest first. Order matters: the loudest one that is due is the one shown.
    pub const ALL: [Self; 2] = [Self::Warning, Self::Danger];

    /// The consumption this threshold is reached at.
    pub fn at(self) -> f64 {
        match self {
            Self::Warning => WARNING_AT,
            Self::Danger => DANGER_AT,
        }
    }
}

/// What a notification is about, and the name its deduplication row is filed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Consumption reached a threshold within the current segment.
    Threshold(Threshold),
    /// The window rolled over. Fired however little of it was spent: providers reset quota
    /// outside their own schedule, and an unscheduled reset is the news.
    Reset,
}

impl Kind {
    /// Every kind, so a caller can ask the dedup table about all of them at once.
    pub const ALL: [Self; 3] = [
        Self::Threshold(Threshold::Warning),
        Self::Threshold(Threshold::Danger),
        Self::Reset,
    ];

    /// The stored name. **Changing one of these re-arms every notification that has
    /// already gone out**, because it is the primary key of the dedup row.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Threshold(Threshold::Warning) => "threshold-70",
            Self::Threshold(Threshold::Danger) => "threshold-90",
            Self::Reset => "reset",
        }
    }
}

/// One notification to send, together with everything it settles.
#[derive(Debug, Clone, PartialEq)]
pub struct Decided {
    /// What to put on screen.
    pub kind: Kind,
    /// Every kind this message stands in for, including itself. All of them are recorded
    /// once it lands — a threshold overtaken by a louder one has been dealt with, and
    /// firing it afterwards would be news from the past.
    pub settles: Vec<Kind>,
}

/// What one reading of one window warrants, given what its segment has already been told.
///
/// `rolled_over` is whether this reading opened a new segment, and `sent` is what has
/// already gone out **for the segment this reading landed in** — which after a rollover is
/// the new one, and therefore empty.
pub fn decide(used_percent: f64, rolled_over: bool, sent: &[Kind]) -> Vec<Decided> {
    let said = |kind: Kind| sent.contains(&kind);
    let mut decided = Vec::new();

    if rolled_over && !said(Kind::Reset) {
        decided.push(Decided {
            kind: Kind::Reset,
            settles: vec![Kind::Reset],
        });
    }

    let due: Vec<Kind> = Threshold::ALL
        .into_iter()
        .filter(|threshold| used_percent >= threshold.at())
        .map(Kind::Threshold)
        .filter(|kind| !said(*kind))
        .collect();
    if let Some(loudest) = due.last().copied() {
        decided.push(Decided {
            kind: loudest,
            settles: due,
        });
    }

    decided
}

/// How hard the notification insists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// Shows and goes away.
    Normal,
    /// Stays until it is dismissed. Only the last warning before a quota runs out earns
    /// this: a notification that must be clicked away is a cost, and spending it on
    /// anything recoverable teaches the user to click it away without reading.
    Critical,
}

impl Urgency {
    /// The value of the `urgency` hint, as freedesktop defines it.
    #[cfg(unix)]
    pub fn hint(self) -> u8 {
        match self {
            Self::Normal => 1,
            Self::Critical => 2,
        }
    }
}

/// One message, ready for the notification server.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    /// The line that carries the news.
    pub summary: String,
    /// The line under it, when there is something true to add.
    pub body: Option<String>,
    /// The provider's own mark, or `None` for a slug that names no installed icon.
    pub icon: Option<String>,
    /// How hard it insists.
    pub urgency: Urgency,
    /// Which window this is about, as one opaque string.
    ///
    /// A notification server is asked to *replace* the message still on screen for the same
    /// window rather than stack a second one under it: "limit reset" after "96% used" is
    /// the same conversation continuing, and two entries in the tray for one window is two
    /// things to dismiss.
    pub about: String,
}

/// Phrases one decision.
///
/// **The event comes first.** How much of the summary a user sees is their desktop's
/// decision, not ours, and what gets cut is the tail — so the fact that this is a reset, or
/// that consumption reached a number, has to be in front of the provider and the window it
/// happened to.
pub fn compose(provider: &str, window: &Window, kind: Kind, now: Timestamp) -> Notice {
    let event = match kind {
        Kind::Threshold(_) => format!("{} used", present::percent(window.used_percent)),
        Kind::Reset => "Limit reset".to_owned(),
    };
    let body = match kind {
        // A reset says one thing and has nothing to add: what the window will do next is
        // on the card, and a second line here would only be something more to read.
        Kind::Reset => None,
        Kind::Threshold(_) => match window.seconds_until_reset(now) {
            None => None,
            Some(0) => Some("Resetting now.".to_owned()),
            Some(seconds) => Some(format!("Resets in {}.", present::duration(seconds))),
        },
    };

    Notice {
        summary: format!("{event} — {} · {}", display_name(provider), window.title),
        body,
        icon: present::icon_name(provider),
        urgency: match kind {
            Kind::Threshold(Threshold::Danger) => Urgency::Critical,
            _ => Urgency::Normal,
        },
        about: format!("{provider}/{}", window.key),
    }
}

/// What the notification calls the provider: the catalog's own spelling of its name when
/// this build knows it, the capitalised slug otherwise.
///
/// The settings dialog already shows the catalog's title — "ClinePass", not "Clinepass" —
/// and a notification that spelled the same provider differently would read as two
/// services.
fn display_name(provider: &str) -> String {
    crate::registry::title(provider)
        .map(str::to_owned)
        .unwrap_or_else(|| provider_label(provider))
}

/// Why a notification did not reach the user.
///
/// Never fatal, and never recorded as delivered: the poll after this one tries again.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// No desktop notification transport accepted the message.
    // The Unix transports raise both of these; the Windows transport (todo 16) will
    // raise neither, and the engine's recorder still returns `Unreachable` in tests on
    // every platform — so the variants are constructed, just not on every target.
    #[allow(dead_code)]
    #[error("no notification server took the message")]
    Unreachable,
    /// The transport took the call and did not answer within [`DELIVERY_TIMEOUT`].
    #[allow(dead_code)]
    #[error("the notification server did not answer in time")]
    Timeout,
    /// This build has no usable desktop notification transport.
    #[cfg(windows)]
    #[error("notification transport unavailable on this build")]
    Unavailable,
}

/// Somewhere for a [`Notice`] to go.
///
/// A trait so the poll loop can be tested without a desktop: the engine's tests run the
/// real decision path against a recorder that can also be told to refuse, which is the
/// only way to exercise "delivery failed, so try again next poll" at all.
pub trait Notifier: std::fmt::Debug + Send + Sync {
    /// Delivers one notification, or says why it could not.
    fn send(&self, notice: &Notice) -> BoxFuture<'_, Result<(), NotifyError>>;
}

/// The shared replacement bookkeeping around the platform notification transport.
#[derive(Debug)]
pub struct Desktop {
    transport: transport::Transport,
    /// The server-assigned id of the message still on screen for each window, so the next
    /// one about that window replaces it. Lost on restart, which only costs one extra
    /// entry in the user's tray.
    showing: Mutex<HashMap<String, u32>>,
}

impl Desktop {
    /// Wraps an existing session-bus connection. The daemon already has one.
    #[cfg(unix)]
    pub fn new(connection: zbus::Connection) -> Self {
        Self {
            transport: transport::Transport::new(connection),
            showing: Mutex::new(HashMap::new()),
        }
    }

    /// The Windows transport stands alone: toasts (todo 16) need no bus connection.
    #[cfg(windows)]
    pub fn new() -> Self {
        Self {
            transport: transport::Transport,
            showing: Mutex::new(HashMap::new()),
        }
    }
}

impl Notifier for Desktop {
    fn send(&self, notice: &Notice) -> BoxFuture<'_, Result<(), NotifyError>> {
        let notice = notice.clone();
        Box::pin(async move {
            let replaces = self
                .showing
                .lock()
                .map(|showing| showing.get(&notice.about).copied())
                .unwrap_or(None);
            let id = self
                .transport
                .send(
                    notice.summary.as_str(),
                    notice.body.as_deref(),
                    notice.icon.as_deref(),
                    notice.urgency,
                    replaces,
                )
                .await?;
            if let Ok(mut showing) = self.showing.lock() {
                showing.insert(notice.about, id);
            }
            Ok(())
        })
    }
}

#[cfg(unix)]
mod transport {
    use super::*;

    /// The session-bus desktop notification transport.
    #[derive(Debug)]
    pub struct Transport {
        connection: zbus::Connection,
    }

    impl Transport {
        pub fn new(connection: zbus::Connection) -> Self {
            Self { connection }
        }

        pub async fn send(
            &self,
            title: &str,
            body: Option<&str>,
            icon: Option<&str>,
            urgency: Urgency,
            replaces: Option<u32>,
        ) -> Result<u32, NotifyError> {
            let mut hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
            hints.insert("urgency", urgency.hint().into());

            let arguments = (
                "Tidemark",
                replaces.unwrap_or(0),
                icon.unwrap_or_default(),
                title,
                body.unwrap_or_default(),
                Vec::<String>::new(),
                hints,
                // The server's own default lifetime. Urgency already says which messages
                // should outlive it, and second-guessing the desktop's timing is not our
                // business.
                -1_i32,
            );
            let call = self.connection.call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &arguments,
            );
            let reply = match tokio::time::timeout(DELIVERY_TIMEOUT, call).await {
                Err(_) => return Err(NotifyError::Timeout),
                Ok(Err(error)) => {
                    tracing::debug!(%error, "the notification server refused the message");
                    return Err(NotifyError::Unreachable);
                }
                Ok(Ok(reply)) => reply,
            };
            reply.body().deserialize::<u32>().map_err(|error| {
                tracing::debug!(%error, "the notification server answered with something else");
                NotifyError::Unreachable
            })
        }
    }
}

#[cfg(windows)]
mod transport {
    use super::*;

    /// The Windows toast transport is implemented in todo 16.
    #[derive(Debug)]
    pub struct Transport;

    impl Transport {
        pub async fn send(
            &self,
            _title: &str,
            _body: Option<&str>,
            _icon: Option<&str>,
            _urgency: Urgency,
            _replaces: Option<u32>,
        ) -> Result<u32, NotifyError> {
            Err(NotifyError::Unavailable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{Timestamp, Window, WindowKey, WindowLength};

    const HOUR: i64 = 3600;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_785_700_000).expect("plausible")
    }

    fn window(used: f64, resets_in: Option<i64>) -> Window {
        let length = WindowLength::from_secs(5 * 3600);
        Window {
            key: WindowKey::for_length(length.expect("nonzero")),
            title: "5 hours".into(),
            subtitle: None,
            used_percent: used,
            resets_at: resets_in.map(|s| now().saturating_add_seconds(s)),
            length,
        }
    }

    fn kinds(decided: &[Decided]) -> Vec<Kind> {
        decided.iter().map(|d| d.kind).collect()
    }

    #[test]
    fn a_window_below_the_first_threshold_says_nothing() {
        assert!(decide(69.9, false, &[]).is_empty());
    }

    #[test]
    fn the_first_threshold_is_due_the_moment_it_is_reached() {
        let decided = decide(70.0, false, &[]);
        assert_eq!(kinds(&decided), vec![Kind::Threshold(Threshold::Warning)]);
        assert_eq!(
            decided[0].settles,
            vec![Kind::Threshold(Threshold::Warning)]
        );
    }

    /// A window first seen at 96% — a fresh install, or a provider that only just started
    /// reporting it. Two pop-ups about one window is noise, so the louder one speaks and
    /// the quieter one is filed as said.
    #[test]
    fn a_reading_that_arrives_past_both_thresholds_interrupts_once() {
        let decided = decide(96.0, false, &[]);
        assert_eq!(kinds(&decided), vec![Kind::Threshold(Threshold::Danger)]);
        assert_eq!(
            decided[0].settles,
            vec![
                Kind::Threshold(Threshold::Warning),
                Kind::Threshold(Threshold::Danger)
            ]
        );
    }

    #[test]
    fn a_threshold_already_said_is_not_said_again() {
        let sent = [Kind::Threshold(Threshold::Warning)];
        assert!(decide(85.0, false, &sent).is_empty());
    }

    #[test]
    fn the_second_threshold_is_still_due_after_the_first_was_said() {
        let sent = [Kind::Threshold(Threshold::Warning)];
        let decided = decide(96.0, false, &sent);
        assert_eq!(kinds(&decided), vec![Kind::Threshold(Threshold::Danger)]);
        assert_eq!(decided[0].settles, vec![Kind::Threshold(Threshold::Danger)]);
    }

    /// Providers reset quota outside their own schedule. Whether one percent or ninety was
    /// spent, the rollover is the news — see `CONTEXT.md` § Notifications.
    #[test]
    fn a_rollover_is_announced_however_little_was_spent() {
        assert_eq!(kinds(&decide(1.0, true, &[])), vec![Kind::Reset]);
    }

    #[test]
    fn a_rollover_already_announced_is_not_announced_again() {
        assert!(decide(1.0, true, &[Kind::Reset]).is_empty());
    }

    #[test]
    fn a_rollover_into_a_window_that_is_already_hot_says_both() {
        let decided = decide(96.0, true, &[]);
        assert_eq!(
            kinds(&decided),
            vec![Kind::Reset, Kind::Threshold(Threshold::Danger)],
            "the rollover is the headline and comes first"
        );
    }

    /// These strings are the primary key of the dedup table. Changing one re-arms every
    /// notification that has already gone out.
    #[test]
    fn every_kind_is_filed_under_a_stable_name() {
        assert_eq!(Kind::Threshold(Threshold::Warning).as_str(), "threshold-70");
        assert_eq!(Kind::Threshold(Threshold::Danger).as_str(), "threshold-90");
        assert_eq!(Kind::Reset.as_str(), "reset");
    }

    /// The event comes first because the tail is what a notification server truncates.
    #[test]
    fn a_threshold_notice_leads_with_the_number() {
        let notice = compose(
            "claude",
            &window(80.0, Some(HOUR + 12 * 60)),
            Kind::Threshold(Threshold::Warning),
            now(),
        );
        assert_eq!(notice.summary, "80% used — Claude · 5 hours");
        assert_eq!(notice.body.as_deref(), Some("Resets in 1 h 12 min."));
    }

    #[test]
    fn a_reset_notice_leads_with_the_word_and_says_nothing_else() {
        let notice = compose("zai", &window(0.0, Some(5 * HOUR)), Kind::Reset, now());
        assert_eq!(notice.summary, "Limit reset — Z.ai · 5 hours");
        assert_eq!(notice.body, None);
    }

    /// Two of the five providers report windows with no reset time at all. Inventing one
    /// to fill the second line would put a confident wrong number on screen.
    #[test]
    fn a_window_with_no_reset_time_has_no_second_line() {
        let notice = compose(
            "kimi",
            &window(96.0, None),
            Kind::Threshold(Threshold::Danger),
            now(),
        );
        assert_eq!(notice.body, None);
    }

    #[test]
    fn an_overdue_reset_is_happening_rather_than_negative() {
        let notice = compose(
            "codex",
            &window(96.0, Some(-600)),
            Kind::Threshold(Threshold::Danger),
            now(),
        );
        assert_eq!(notice.body.as_deref(), Some("Resetting now."));
    }

    #[test]
    fn the_number_on_the_notice_is_the_reading_not_the_threshold_it_crossed() {
        let notice = compose(
            "claude",
            &window(96.4, None),
            Kind::Threshold(Threshold::Danger),
            now(),
        );
        assert!(notice.summary.starts_with("96% used"), "{}", notice.summary);
    }

    /// One window, one entry in the tray: the reset replaces the warning it follows.
    #[test]
    fn every_notice_about_one_window_carries_the_same_identity() {
        let warning = compose(
            "claude",
            &window(96.0, None),
            Kind::Threshold(Threshold::Danger),
            now(),
        );
        let reset = compose("claude", &window(0.0, None), Kind::Reset, now());
        assert_eq!(warning.about, reset.about);

        let other = Window {
            key: WindowKey::named("w604800"),
            ..window(96.0, None)
        };
        assert_ne!(
            compose("claude", &other, Kind::Reset, now()).about,
            reset.about
        );
    }

    #[test]
    fn every_kind_has_a_name_of_its_own() {
        let names: std::collections::BTreeSet<&str> =
            Kind::ALL.iter().map(|kind| kind.as_str()).collect();
        assert_eq!(names.len(), Kind::ALL.len());
    }

    #[test]
    fn the_notice_carries_the_providers_own_mark() {
        let notice = compose(
            "claude",
            &window(80.0, None),
            Kind::Threshold(Threshold::Warning),
            now(),
        );
        assert_eq!(notice.icon.as_deref(), Some("tidemark-claude-symbolic"));
    }

    /// Only the last warning before the quota runs out is allowed to stay on screen.
    #[test]
    fn the_first_threshold_does_not_demand_attention_and_the_second_does() {
        assert_eq!(
            compose(
                "claude",
                &window(80.0, None),
                Kind::Threshold(Threshold::Warning),
                now()
            )
            .urgency,
            Urgency::Normal
        );
        assert_eq!(
            compose(
                "claude",
                &window(96.0, None),
                Kind::Threshold(Threshold::Danger),
                now()
            )
            .urgency,
            Urgency::Critical
        );
        assert_eq!(
            compose("claude", &window(1.0, None), Kind::Reset, now()).urgency,
            Urgency::Normal
        );
    }
}
