//! One fetch result, and the identities it is filed under.

use crate::time::Timestamp;
use crate::window::{Window, WindowLength};
use serde::{Deserialize, Serialize};
use zvariant::Type;

/// One AI service. The string is a stable slug — `claude`, `codex`, `zai`, `kimi`,
/// `antigravity` — used as a storage key and in config, so it never changes once shipped.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(String);

/// One set of credentials for a provider.
///
/// v1 shows exactly one account per provider, but every stored key carries an account
/// component from day one, so multi-account becomes a change to the interface rather than
/// a migration of the history.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(String);

macro_rules! slug_newtype {
    ($name:ident, $default:expr) => {
        impl $name {
            #[doc = "Wraps a slug."]
            pub fn new(slug: impl Into<String>) -> Self {
                Self(slug.into())
            }

            #[doc = "The slug as stored."]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self($default.to_owned())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

slug_newtype!(ProviderId, "unknown");
slug_newtype!(AccountId, "default");

impl ProviderId {
    /// What to call this provider in front of a person. See [`provider_label`].
    pub fn label(&self) -> String {
        provider_label(&self.0)
    }
}

/// A labelled value that does not fit the window model — Kimi's absolute request counts,
/// Codex's reset credits, a plan name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct DetailRow {
    /// Left-hand label.
    pub label: String,
    /// Right-hand value, already formatted by the provider adapter.
    pub value: String,
}

/// A titled group of [`DetailRow`]s, shown in the detail dialog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct DetailSection {
    /// Section heading.
    pub title: String,
    /// Rows, in the order they should be shown.
    pub rows: Vec<DetailRow>,
}

impl DetailSection {
    /// The heading a provider files its subscription level under.
    ///
    /// A convention rather than a field, because "plan" means something different at every
    /// provider — a level, a tier, a seat, a credit balance — and the adapters already
    /// phrase it in the provider's own words. What a client needs is only *which section*
    /// to lift onto the card, so that is all this pins down. See [`crate::ProviderStatus::plan`].
    pub const PLAN: &'static str = "Plan";
}

/// What to call a provider in front of a person.
///
/// Presentation, but shared: the card, the tray menu and the notification text must all
/// say "Z.ai" rather than `zai`, and they run in two different processes. An unknown slug
/// is capitalised rather than refused — a newer daemon may watch a provider this build has
/// never heard of, and its card is still worth drawing.
pub fn provider_label(slug: &str) -> String {
    match slug {
        "claude" => "Claude".to_owned(),
        "codex" => "Codex".to_owned(),
        "zai" => "Z.ai".to_owned(),
        "kimi" => "Kimi".to_owned(),
        "antigravity" => "Antigravity".to_owned(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Unknown".to_owned(),
            }
        }
    }
}

/// Everything one poll of one account produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Which service.
    pub provider: ProviderId,
    /// Which credentials.
    pub account: AccountId,
    /// When the fetch completed.
    pub captured_at: Timestamp,
    /// Every window the provider reported. A provider returns however many it wants, and
    /// the set may change between responses; a window that is absent is simply not drawn.
    pub windows: Vec<Window>,
    /// Anything that does not fit the window model.
    pub details: Vec<DetailSection>,
}

impl Snapshot {
    /// The window the card leads with: the shortest one *present*.
    ///
    /// Shortest, because that is the limit a user is about to hit. Present, because a
    /// window the provider did not report this time is not drawn at all — OpenAI switched
    /// its five-hour window off and could switch it back on, and neither transition should
    /// leave a placeholder behind.
    ///
    /// Windows without a declared length sort last: an unknown length cannot be claimed to
    /// be the shortest.
    pub fn dominant_window(&self) -> Option<&Window> {
        self.windows
            .iter()
            .min_by_key(|w| (w.length.is_none(), w.length.map(WindowLength::as_secs)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::WindowKey;

    fn snapshot(lengths: &[Option<u64>]) -> Snapshot {
        Snapshot {
            provider: ProviderId::new("test"),
            account: AccountId::default(),
            captured_at: Timestamp::from_unix(1_785_700_000).expect("plausible"),
            windows: lengths
                .iter()
                .map(|len| Window {
                    key: WindowKey::named(&format!("{len:?}")),
                    title: format!("{len:?}"),
                    used_percent: 0.0,
                    resets_at: None,
                    length: len.and_then(WindowLength::from_secs),
                })
                .collect(),
            details: Vec::new(),
        }
    }

    #[test]
    fn the_dominant_window_is_the_shortest_one() {
        let s = snapshot(&[Some(604_800), Some(18_000), Some(2_592_000)]);
        assert_eq!(
            s.dominant_window().expect("present").length,
            WindowLength::from_secs(18_000)
        );
    }

    #[test]
    fn a_window_of_unknown_length_never_claims_to_be_the_shortest() {
        let s = snapshot(&[None, Some(604_800)]);
        assert_eq!(
            s.dominant_window().expect("present").length,
            WindowLength::from_secs(604_800)
        );
    }

    #[test]
    fn a_provider_reporting_nothing_has_no_dominant_window() {
        assert!(snapshot(&[]).dominant_window().is_none());
    }

    #[test]
    fn unknown_lengths_still_yield_a_dominant_window_when_they_are_all_there_is() {
        let s = snapshot(&[None, None]);
        assert!(s.dominant_window().is_some());
    }

    #[test]
    fn known_providers_are_spelled_the_way_they_spell_themselves() {
        assert_eq!(ProviderId::new("zai").label(), "Z.ai");
        assert_eq!(provider_label("antigravity"), "Antigravity");
    }

    #[test]
    fn a_provider_this_build_has_never_heard_of_still_gets_a_name() {
        // A newer daemon may publish a slug we do not know. Capitalising it beats
        // showing the raw slug, and beats refusing to draw the card at all.
        assert_eq!(provider_label("mistral"), "Mistral");
        assert_eq!(provider_label(""), "Unknown");
    }

    #[test]
    fn the_default_account_is_named_not_empty() {
        assert_eq!(AccountId::default().as_str(), "default");
    }
}
