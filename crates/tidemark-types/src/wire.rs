//! What the daemon publishes on D-Bus, and what its clients read.
//!
//! # Why these are not the domain types
//!
//! [`Snapshot`] answers "what did the provider say". A client needs one more thing: *why
//! there is no snapshot* — no key stored yet, keyring still locked, the provider rejected
//! the credential, a 429. Those are states of the account, not readings of it, so the
//! published shape is a [`ProviderStatus`] that carries a [`ProviderState`] and, when
//! there is one, the last good reading underneath it.
//!
//! # Why dictionaries and not structs
//!
//! Every wire struct here encodes as `a{sv}`. That buys two things a fixed D-Bus struct
//! signature cannot:
//!
//! * **Absent means absent.** D-Bus has no optional field, so a fixed struct would have to
//!   put a sentinel — `0`, `-1` — where a provider said nothing. This project has one rule
//!   it will not bend: a window with no reset time is normal, and inventing a value to fill
//!   the slot puts a confident wrong number in front of the user. A missing dictionary key
//!   cannot be mistaken for a real one.
//! * **Adding a field is not a breaking change.** The interface is designed as if a CLI and
//!   a Waybar module were already consuming it. They will be built against whatever exists
//!   when they are written, and a new key must not stop them parsing.
//!
//! The same derives give serde a map, so `tidemark usage --json` is the same struct
//! serialized to JSON rather than a second definition to keep in step.

use crate::snapshot::{AccountId, DetailSection, ProviderId, Snapshot};
use crate::time::Timestamp;
use crate::window::{Window, WindowKey, WindowLength};
use zvariant::{DeserializeDict, SerializeDict, Type};

/// What is currently true of one account.
///
/// The variants exist to be acted on. The interface collapses them into three groups by
/// what the user must do — see [`ProviderState::remedy`] — but the daemon keeps them apart,
/// because "no key saved" and "the key was rejected" are one dialog away from each other
/// and a log that cannot tell them apart is useless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderState {
    /// Configured, but nothing has come back yet. The state at startup.
    Pending,
    /// The last poll produced a snapshot.
    Ok,
    /// No credential is stored for this account.
    NoCredential,
    /// The Secret Service is running and locked. Not a failure: the user unlocks the
    /// keyring in their own time, and the daemon keeps asking. See `tidemark_core::secrets`.
    WaitingForKeyring,
    /// No Secret Service answered at all.
    KeyringUnavailable,
    /// The provider rejected the credential.
    CredentialRejected,
    /// The provider asked us to slow down.
    RateLimited,
    /// The request did not complete, or the provider answered with a server error.
    Unreachable,
    /// The response arrived and did not mean what we expect it to mean.
    Malformed,
}

/// What the user can do about a state. `CONTEXT.md` § Interface: failure states are
/// distinguished in the data and collapsed in the interface into these groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Remedy {
    /// Nothing is wrong.
    Nothing,
    /// The user must act: store a key, replace a rejected one, run a keyring.
    YouFixIt,
    /// Waiting is the correct response: a rate limit expires, a network returns, a
    /// keyring gets unlocked at login.
    ItFixesItself,
    /// The provider changed something under us. Only a new release fixes it.
    TheyBrokeIt,
}

impl ProviderState {
    /// The string this state travels as. Stable: it is matched by clients and appears in
    /// `busctl` output.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ok => "ok",
            Self::NoCredential => "no-credential",
            Self::WaitingForKeyring => "waiting-for-keyring",
            Self::KeyringUnavailable => "keyring-unavailable",
            Self::CredentialRejected => "credential-rejected",
            Self::RateLimited => "rate-limited",
            Self::Unreachable => "unreachable",
            Self::Malformed => "malformed",
        }
    }

    /// Parses a state off the wire. `None` for anything this build does not know, which a
    /// client should show as an unexplained problem rather than as health.
    pub fn from_wire(value: &str) -> Option<Self> {
        [
            Self::Pending,
            Self::Ok,
            Self::NoCredential,
            Self::WaitingForKeyring,
            Self::KeyringUnavailable,
            Self::CredentialRejected,
            Self::RateLimited,
            Self::Unreachable,
            Self::Malformed,
        ]
        .into_iter()
        .find(|candidate| candidate.as_wire() == value)
    }

    /// Which of the three groups the interface shows this as.
    pub const fn remedy(self) -> Remedy {
        match self {
            Self::Ok | Self::Pending => Remedy::Nothing,
            Self::NoCredential | Self::KeyringUnavailable | Self::CredentialRejected => {
                Remedy::YouFixIt
            }
            Self::WaitingForKeyring | Self::RateLimited | Self::Unreachable => {
                Remedy::ItFixesItself
            }
            Self::Malformed => Remedy::TheyBrokeIt,
        }
    }
}

impl std::fmt::Display for ProviderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// How an account is authenticated, and therefore what the interface offers.
///
/// Published rather than inferred by the client: the credentials dialog is the one place
/// where "paste a key" and "sign in" are genuinely different screens, and a client that
/// guessed from the provider slug would have to be taught every new provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// The user pastes a key, which Tidemark keeps in the Secret Service.
    Key,
    /// The user signs in through the browser, and Tidemark keeps the tokens.
    OAuth,
    /// Neither: the credential belongs to something else on the machine, and Tidemark
    /// reads it where that thing keeps it. Nothing to enter and nothing to sign out of.
    External,
    /// No credential at all: the provider answers anyone who can reach it, which in
    /// practice means a service already running on this machine. Configuring the account
    /// is its settings alone, and there is nothing for a credentials dialog to offer.
    None,
}

impl CredentialKind {
    /// The string this kind travels as.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::OAuth => "oauth",
            Self::External => "external",
            Self::None => "none",
        }
    }

    /// Parses a kind off the wire. `None` for anything this build does not know, which a
    /// client should treat as "no credential interface" rather than guessing at one.
    pub fn from_wire(value: &str) -> Option<Self> {
        [Self::Key, Self::OAuth, Self::External, Self::None]
            .into_iter()
            .find(|candidate| candidate.as_wire() == value)
    }
}

impl std::fmt::Display for CredentialKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// One value a provider setting can take.
#[derive(Debug, Clone, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct OptionChoice {
    /// What it is called on the wire and in `config.toml`.
    pub value: String,
    /// What it is called on screen.
    pub title: String,
}

/// A setting of a provider that is neither a secret nor a reading.
///
/// Z.ai is the reason this exists: the same API answers on two hosts, a key for one is a
/// 401 on the other, and nothing in the key says which. The choice is published with its
/// alternatives so that a client can draw the control without knowing what a region is —
/// the same reason the states are published as strings with a documented set.
#[derive(Debug, Clone, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct ProviderOption {
    /// Key under `[provider.<slug>]` in `config.toml`.
    pub name: String,
    /// What to call it on screen.
    pub title: String,
    /// A sentence under the control, when it needs one.
    pub description: Option<String>,
    /// What it is currently set to.
    pub value: String,
    /// Everything it may be set to. Empty means free text — a base URL has no menu to
    /// offer.
    pub choices: Vec<OptionChoice>,
}

/// The local CLI login a provider can read instead of a login performed in Tidemark.
///
/// Three of the providers have two credentials rather than one: a token Tidemark obtained
/// itself, and the token some other program on this machine already holds. Which of the
/// two is used was, for a long time, decided silently in the daemon. It is published here
/// instead, in enough detail for a client to *say* what the other credential is, where it
/// lives, how to create one, and whether Tidemark writes to it — because a program that
/// reads and refreshes a file another program owns must say so where the user can see it.
#[derive(Debug, Clone, PartialEq, Eq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct ExternalLogin {
    /// The setting that chooses between the two credentials, under `[provider.<slug>]`.
    /// It is also one of [`ProviderDefinition::options`], and a client that draws the
    /// choice itself must not draw it a second time among the ordinary settings.
    pub option: String,
    /// What the local login is called, in the words its own program uses — `Claude Code
    /// login`, `agy session`.
    pub label: String,
    /// Where it lives, for a person: a path, or a sentence about a running process.
    pub location: String,
    /// What to run to create one. Empty when there is nothing single to name.
    pub command: String,
    /// Whether Tidemark refreshes this credential in place and writes the rotated token
    /// back where it found it, per ADR 0001. `false` for a source Tidemark only reads.
    pub writes_back: bool,
}

/// One top-level local authentication method a provider can offer.
///
/// Values are stable configuration values. A client draws `title`, but sends `value` back
/// to the daemon unchanged so that it never needs to know provider-specific source names.
#[derive(Debug, Clone, PartialEq, Eq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct AuthMode {
    /// Stable value stored in the provider configuration.
    pub value: String,
    /// What to call this method on screen.
    pub title: String,
}

/// Metadata for a provider's local authentication source selector.
#[derive(Debug, Clone, PartialEq, Eq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct AuthSelector {
    /// The provider option that records the selected top-level mode.
    pub option: String,
    /// Top-level methods the client presents as authentication tabs.
    pub modes: Vec<AuthMode>,
}

/// The daemon's most recent verdict on a local authentication candidate.
///
/// The state travels as a string on [`AuthCandidate`] so a newer daemon can add a verdict
/// without changing its D-Bus signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthCandidateState {
    /// The daemon proved that this source authenticates successfully.
    Ready,
    /// The source does not contain a usable matching credential.
    Missing,
    /// The provider rejected the source's credential.
    Rejected,
    /// A locked keyring prevented the daemon from reading the source.
    WaitingForKeyring,
    /// The daemon could not reach the provider to determine whether the source works.
    Unreachable,
}

impl AuthCandidateState {
    /// The stable string this state travels as.
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Missing => "missing",
            Self::Rejected => "rejected",
            Self::WaitingForKeyring => "waiting-for-keyring",
            Self::Unreachable => "unreachable",
        }
    }

    /// Parses a daemon verdict. Unknown values remain unknown rather than being shown as a
    /// usable source by an older client.
    pub fn from_wire(value: &str) -> Option<Self> {
        [
            Self::Ready,
            Self::Missing,
            Self::Rejected,
            Self::WaitingForKeyring,
            Self::Unreachable,
        ]
        .into_iter()
        .find(|candidate| candidate.as_wire() == value)
    }
}

impl std::fmt::Display for AuthCandidateState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// One local source the daemon inspected, without its credential.
///
/// `id` is opaque to clients and stable enough to record a selected browser or profile.
/// Children represent the rare case where a source needs a second explicit choice, such as
/// multiple usable profiles inside a browser.
#[derive(Debug, Clone, PartialEq, Eq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct AuthCandidate {
    /// Stable opaque source identifier.
    pub id: String,
    /// Human-readable source name.
    pub title: String,
    /// Optional human-readable context, never a filesystem path or credential.
    pub subtitle: Option<String>,
    /// An [`AuthCandidateState`] string.
    pub state: String,
    /// Nested choices within this source.
    pub children: Vec<AuthCandidate>,
}

impl AuthCandidate {
    /// The verdict this build recognizes, if any.
    pub fn state(&self) -> Option<AuthCandidateState> {
        AuthCandidateState::from_wire(&self.state)
    }
}

/// The explicit local authentication source an account uses.
#[derive(Debug, Clone, PartialEq, Eq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct AuthSelection {
    /// The selected [`AuthMode::value`].
    pub mode: String,
    /// The selected candidate, or none for a mode that has no candidate choice.
    pub candidate: Option<String>,
}

/// Presentation metadata for one provider in the daemon's catalog.
#[derive(Debug, Clone, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct ProviderDefinition {
    pub provider: String,
    pub title: String,
    pub credential: String,
    pub credential_hint: String,
    /// The other credential this provider can read, for the providers that have one.
    pub external: Option<ExternalLogin>,
    /// An explicit local browser or application authentication selector, when the provider
    /// supports one. Its dynamic candidates are fetched separately from the daemon.
    pub browser_auth: Option<AuthSelector>,
    pub options: Vec<ProviderOption>,
}

impl ProviderDefinition {
    pub fn credential_kind(&self) -> Option<CredentialKind> {
        CredentialKind::from_wire(&self.credential)
    }

    /// The setting that chooses between this provider's two credentials, when it has two.
    pub fn auth_option(&self) -> Option<&str> {
        self.external
            .as_ref()
            .map(|external| external.option.as_str())
    }
}

/// One rate-limit window as published.
///
/// `resets_at` and `length_secs` are absent from the encoded dictionary when the provider
/// did not say — see the module docs. A client that wants a pace mark rebuilds the domain
/// [`Window`] with [`WindowStatus::to_window`] and asks it.
#[derive(Debug, Clone, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct WindowStatus {
    /// Stable identity, derived from the window's length rather than its position in the
    /// provider's response. See [`WindowKey`].
    pub key: String,
    /// What to call it, in the provider's own terms.
    pub title: String,
    /// The absolute quantities behind `used_percent`, already formatted by the provider.
    /// Absent when the provider reported only a percentage. See [`Window::subtitle`].
    pub subtitle: Option<String>,
    /// Consumption, 0..=100.
    pub used_percent: f64,
    /// Unix seconds of the next rollover, when the provider said.
    pub resets_at: Option<i64>,
    /// Window length in seconds, when the provider said or it could be derived.
    pub length_secs: Option<u64>,
}

impl WindowStatus {
    /// Publishes a domain window.
    pub fn from_window(window: &Window) -> Self {
        Self {
            key: window.key.to_string(),
            title: window.title.clone(),
            subtitle: window.subtitle.clone(),
            used_percent: window.used_percent,
            resets_at: window.resets_at.map(Timestamp::as_unix),
            length_secs: window.length.map(WindowLength::as_secs),
        }
    }

    /// Rebuilds the domain window, so a client gets [`Window::pace`] rather than
    /// reimplementing it.
    ///
    /// A `resets_at` that fails [`Timestamp::from_unix`] is dropped rather than refused:
    /// the rest of the window is still worth drawing, and the pace mark simply does not
    /// appear — which is a state the interface already has to render.
    pub fn to_window(&self) -> Window {
        Window {
            key: WindowKey::named(&self.key),
            title: self.title.clone(),
            subtitle: self.subtitle.clone(),
            used_percent: self.used_percent,
            resets_at: self.resets_at.and_then(|s| Timestamp::from_unix(s).ok()),
            length: self.length_secs.and_then(WindowLength::from_secs),
        }
    }
}

/// Paths and storage facts shown on the Preferences data page.
#[derive(Debug, Clone, PartialEq, Eq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct DataInfo {
    pub config_path: String,
    pub history_path: String,
    pub history_bytes: u64,
    pub key_schema: String,
    pub token_schema: String,
    /// False when a distribution built the daemon without its GitHub release checker.
    pub release_check_available: bool,
}

/// Application preferences kept by the daemon in `config.toml`.
///
/// Strings are used for named choices so a newer daemon can add one without changing the
/// D-Bus signature. Unknown choices are rejected by the daemon rather than guessed by a
/// client.
#[derive(Debug, Clone, PartialEq, Eq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct Preferences {
    /// Whether the daemon may ask GitHub for the latest release.
    pub release_check: bool,
    /// Whether the window's close button hides it when a tray icon can bring it back.
    pub minimize_on_close: bool,
    /// `app`, `daemon`, or `off`: the one coherent login-start behavior.
    pub startup_mode: String,
    /// `forever`, `six-months`, or `one-year`.
    pub history_retention: String,
    /// `off`, `http`, `https` or `socks5`: which kind of proxy every outbound request
    /// and every child process the daemon starts goes through.
    ///
    /// `off` is not the same as "no proxy": it means nothing is configured here, and the
    /// daemon's own environment — `HTTPS_PROXY` and friends — is left in charge, which is
    /// what it was before this setting existed.
    pub proxy_mode: String,
    /// Host name or address of the proxy. Empty until one is set.
    pub proxy_host: String,
    /// Port the proxy listens on. Zero until one is set; a mode other than `off` needs a
    /// real one.
    pub proxy_port: u16,
}

impl Preferences {
    pub const STARTUP_APP: &'static str = "app";
    pub const STARTUP_DAEMON: &'static str = "daemon";
    pub const STARTUP_OFF: &'static str = "off";

    pub const RETENTION_FOREVER: &'static str = "forever";
    pub const RETENTION_SIX_MONTHS: &'static str = "six-months";
    pub const RETENTION_ONE_YEAR: &'static str = "one-year";

    pub const PROXY_OFF: &'static str = "off";
    pub const PROXY_HTTP: &'static str = "http";
    pub const PROXY_HTTPS: &'static str = "https";
    pub const PROXY_SOCKS5: &'static str = "socks5";

    /// Whether this build knows the named startup mode.
    pub fn valid_startup(value: &str) -> bool {
        matches!(
            value,
            Self::STARTUP_APP | Self::STARTUP_DAEMON | Self::STARTUP_OFF
        )
    }

    /// Whether this build knows the named retention policy.
    pub fn valid_retention(value: &str) -> bool {
        matches!(
            value,
            Self::RETENTION_FOREVER | Self::RETENTION_SIX_MONTHS | Self::RETENTION_ONE_YEAR
        )
    }

    /// Whether this build knows the named proxy mode.
    pub fn valid_proxy_mode(value: &str) -> bool {
        matches!(
            value,
            Self::PROXY_OFF | Self::PROXY_HTTP | Self::PROXY_HTTPS | Self::PROXY_SOCKS5
        )
    }
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            release_check: true,
            minimize_on_close: true,
            startup_mode: Self::STARTUP_APP.into(),
            history_retention: Self::RETENTION_FOREVER.into(),
            proxy_mode: Self::PROXY_OFF.into(),
            proxy_host: String::new(),
            proxy_port: 0,
        }
    }
}

/// One stored measurement from the current segment of a rate-limit window.
///
/// The daemon, rather than a client, owns the database that holds these rows. This compact
/// wire value is deliberately separate from the storage type so the GUI never learns about
/// SQLite or the historical reset-time column it does not need to draw consumption.
#[derive(Debug, Clone, Copy, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct HistoryPoint {
    /// Unix seconds when the provider reading completed.
    pub captured_at: i64,
    /// Percent of the selected window consumed at that moment, in 0..=100.
    pub used_percent: f64,
}

/// Everything the daemon currently knows about one account.
#[derive(Debug, Clone, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "a{sv}")]
pub struct ProviderStatus {
    /// Provider slug.
    pub provider: String,
    /// Account slug.
    pub account: String,
    /// A [`ProviderState`] as a string.
    pub state: String,
    /// Human-readable detail for a state that is not `ok`. Absent when there is nothing
    /// to add beyond the state itself.
    pub message: Option<String>,
    /// When the reading below was taken. Absent while the account has never been polled
    /// successfully — a status can carry a state and no reading at all.
    pub captured_at: Option<i64>,
    /// When the daemon intends to poll next, so a client can say "in 4 minutes" without
    /// having to model the scheduler.
    pub next_poll_at: Option<i64>,
    /// The windows of the last good reading. **These survive a failed poll**: a card that
    /// blanked every time a request timed out would be less informative than one showing
    /// the last known numbers next to a state chip.
    pub windows: Vec<WindowStatus>,
    /// Everything from the last good reading that does not fit the window model.
    pub details: Vec<DetailSection>,
    /// A [`CredentialKind`] as a string, saying what the credentials dialog should offer
    /// for this account. Absent from a daemon older than the credentials interface.
    pub credential: Option<String>,
    /// Whether Tidemark itself holds a credential for this account — a stored key, or the
    /// tokens of a login performed here. **Not** whether the account works: a Claude
    /// account reading the CLI's own file is `false` and perfectly healthy.
    pub has_credential: Option<bool>,
    /// Where the user gets the credential from, in one sentence. Absent when the answer is
    /// obvious enough not to need one.
    pub credential_hint: Option<String>,
    /// Whether the local CLI login named by [`ProviderDefinition::external`] exists on
    /// this machine. Absent when the provider has no such login, or when the daemon could
    /// not tell — which is not the same as "there is none", and must not be drawn as one.
    pub external_present: Option<bool>,
    /// Which of the two credentials the next poll will actually use, as an
    /// [`ExternalLogin`] choice value — `oauth` or `cli`.
    ///
    /// Published rather than derived by the client, and deliberately separate from the
    /// choosing setting's own value: that setting may legitimately be unset, and what an
    /// unset setting resolves to is the provider's business. Antigravity's local server is
    /// the session the user is working in and wins by default; Claude's and Codex's own
    /// login wins over the file their CLI owns. A client that guessed would have to be
    /// taught each of those rules, and would go stale the first time one changed.
    pub auth_source: Option<String>,
    /// The daemon-resolved local source selected for browser-cookie authentication.
    /// Absent on providers without this capability and when speaking to an older daemon.
    pub auth_selection: Option<AuthSelection>,
    /// The provider's own settings, with their current values and alternatives.
    pub options: Vec<ProviderOption>,
    /// Keys of the windows whose notifications the user has switched on.
    ///
    /// Empty is the state a freshly added provider is in: notifications are opted into per
    /// window, because five providers reporting three windows each would otherwise be
    /// fifteen sources of interruption nobody asked for. See `CONTEXT.md` § Notifications.
    pub notify: Vec<String>,
}

impl ProviderStatus {
    /// A configured account nothing has been heard from yet.
    pub fn pending(provider: &ProviderId, account: &AccountId) -> Self {
        Self {
            provider: provider.to_string(),
            account: account.to_string(),
            state: ProviderState::Pending.as_wire().to_owned(),
            message: None,
            captured_at: None,
            next_poll_at: None,
            windows: Vec::new(),
            details: Vec::new(),
            credential: None,
            has_credential: None,
            credential_hint: None,
            external_present: None,
            auth_source: None,
            auth_selection: None,
            options: Vec::new(),
            notify: Vec::new(),
        }
    }

    /// What the credentials dialog should offer, or `None` where this build does not know
    /// what the daemon is describing.
    pub fn credential_kind(&self) -> Option<CredentialKind> {
        CredentialKind::from_wire(self.credential.as_deref()?)
    }

    /// The subscription level to show under the provider's name, when there is one.
    ///
    /// By convention the first row of the section a provider titles
    /// [`DetailSection::PLAN`] — see that constant for why this is a convention rather
    /// than a field of its own.
    pub fn plan(&self) -> Option<&str> {
        self.details
            .iter()
            .find(|section| section.title == DetailSection::PLAN)
            .and_then(|section| section.rows.first())
            .map(|row| row.value.as_str())
    }

    /// An amount-only balance to show instead of an invented percentage.
    ///
    /// By convention this is the first row of [`DetailSection::BALANCE`]. It remains detail
    /// data on the wire, so older clients simply keep it in the detail dialog.
    pub fn balance(&self) -> Option<&str> {
        self.details
            .iter()
            .find(|section| section.title == DetailSection::BALANCE)
            .and_then(|section| section.rows.first())
            .map(|row| row.value.as_str())
    }

    /// The state, or `None` if this build does not know the string a newer daemon sent.
    pub fn state(&self) -> Option<ProviderState> {
        ProviderState::from_wire(&self.state)
    }

    /// Replaces the state and its explanation, leaving the last good reading in place.
    pub fn set_state(&mut self, state: ProviderState, message: Option<String>) {
        self.state = state.as_wire().to_owned();
        self.message = message;
    }

    /// Replaces the reading, and sets the state to [`ProviderState::Ok`].
    pub fn set_reading(&mut self, snapshot: &Snapshot) {
        self.captured_at = Some(snapshot.captured_at.as_unix());
        self.windows = snapshot
            .windows
            .iter()
            .map(WindowStatus::from_window)
            .collect();
        self.details = snapshot.details.clone();
        self.set_state(ProviderState::Ok, None);
    }

    /// The reading as a domain [`Snapshot`], or `None` when there has never been one.
    pub fn to_snapshot(&self) -> Option<Snapshot> {
        let captured_at = Timestamp::from_unix(self.captured_at?).ok()?;
        Some(Snapshot {
            provider: ProviderId::new(self.provider.clone()),
            account: AccountId::new(self.account.clone()),
            captured_at,
            windows: self.windows.iter().map(WindowStatus::to_window).collect(),
            details: self.details.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::DetailRow;
    use std::collections::HashMap;
    use zvariant::serialized::{Context, Data};
    use zvariant::{LE, OwnedValue, to_bytes};

    fn window(resets_at: Option<i64>) -> Window {
        Window {
            key: WindowKey::named("w18000"),
            title: "5 hours".into(),
            subtitle: None,
            used_percent: 42.5,
            resets_at: resets_at.map(|s| Timestamp::from_unix(s).expect("plausible")),
            length: WindowLength::from_secs(18_000),
        }
    }

    fn status() -> ProviderStatus {
        let mut status = ProviderStatus::pending(&ProviderId::new("zai"), &AccountId::default());
        status.set_reading(&Snapshot {
            provider: ProviderId::new("zai"),
            account: AccountId::default(),
            captured_at: Timestamp::from_unix(1_785_700_000).expect("plausible"),
            windows: vec![window(Some(1_785_717_000)), window(None)],
            details: vec![DetailSection {
                title: "Plan".into(),
                rows: vec![DetailRow {
                    label: "Level".into(),
                    value: "pro".into(),
                }],
            }],
        });
        status.next_poll_at = Some(1_785_700_300);
        status
    }

    fn encode(status: &ProviderStatus) -> Data<'static, 'static> {
        to_bytes(Context::new_dbus(LE, 0), status).expect("the published shape encodes")
    }

    #[test]
    fn a_provider_definition_survives_the_bus() {
        let original = ProviderDefinition {
            provider: "antigravity".into(),
            title: "Antigravity".into(),
            credential: CredentialKind::OAuth.as_wire().into(),
            credential_hint: "Sign in with Google.".into(),
            external: Some(ExternalLogin {
                option: "source".into(),
                label: "agy session".into(),
                location: "a running agy server".into(),
                command: "agy".into(),
                writes_back: false,
            }),
            browser_auth: None,
            options: Vec::new(),
        };
        let encoded = to_bytes(Context::new_dbus(LE, 0), &original).expect("encodes");
        let (decoded, _): (ProviderDefinition, _) = encoded.deserialize().expect("decodes");
        assert_eq!(decoded, original);
        assert_eq!(decoded.credential_kind(), Some(CredentialKind::OAuth));
        assert_eq!(decoded.auth_option(), Some("source"));
    }

    #[test]
    fn a_browser_auth_definition_and_nested_candidate_survive_the_bus() {
        // Removing the selector or flattening a browser's two profiles would leave the GTK
        // client unable to offer the explicit source the daemon validated.
        let selector = AuthSelector {
            option: "auth-source".into(),
            modes: vec![
                AuthMode {
                    value: "cursor-app".into(),
                    title: "Cursor App".into(),
                },
                AuthMode {
                    value: "browser".into(),
                    title: "Browser".into(),
                },
            ],
        };
        let definition = ProviderDefinition {
            provider: "cursor".into(),
            title: "Cursor".into(),
            credential: CredentialKind::None.as_wire().into(),
            credential_hint: "Choose a local Cursor session.".into(),
            external: None,
            browser_auth: Some(selector.clone()),
            options: Vec::new(),
        };
        let candidate = AuthCandidate {
            id: "firefox".into(),
            title: "Firefox".into(),
            subtitle: None,
            state: AuthCandidateState::Ready.as_wire().into(),
            children: vec![AuthCandidate {
                id: "default-release".into(),
                title: "Default Release".into(),
                subtitle: Some("Cursor session".into()),
                state: AuthCandidateState::Ready.as_wire().into(),
                children: Vec::new(),
            }],
        };

        let encoded = to_bytes(Context::new_dbus(LE, 0), &definition).expect("encodes");
        let (decoded, _): (ProviderDefinition, _) = encoded.deserialize().expect("decodes");
        let encoded = to_bytes(Context::new_dbus(LE, 0), &candidate).expect("encodes");
        let (decoded_candidate, _): (AuthCandidate, _) = encoded.deserialize().expect("decodes");
        let mut status = ProviderStatus::pending(&ProviderId::new("cursor"), &AccountId::default());
        status.auth_selection = Some(AuthSelection {
            mode: "browser".into(),
            candidate: Some("firefox/default-release".into()),
        });
        let (decoded_status, _): (ProviderStatus, _) =
            encode(&status).deserialize().expect("decodes");

        assert_eq!(decoded.browser_auth.as_ref(), Some(&selector));
        assert_eq!(decoded_candidate, candidate);
        assert_eq!(decoded_status.auth_selection, status.auth_selection);
    }

    #[test]
    fn an_older_status_dictionary_without_browser_auth_fields_still_decodes() {
        // A daemon that predates source selection omits this key entirely; the GUI must not
        // mistake that absence for a selected source.
        let original = ProviderStatus::pending(&ProviderId::new("cursor"), &AccountId::default());
        let encoded = encode(&original);
        let (decoded, _): (ProviderStatus, _) = encoded.deserialize().expect("decodes");

        assert_eq!(decoded.auth_selection, None);
    }

    #[test]
    fn a_provider_with_one_credential_names_no_choice() {
        // The absent field is the whole signal a client dispatches on: no external login
        // means no source pill, and a key provider must never grow one.
        let definition = ProviderDefinition {
            provider: "zai".into(),
            title: "Z.ai".into(),
            credential: CredentialKind::Key.as_wire().into(),
            credential_hint: "Z.ai dashboard → API keys.".into(),
            external: None,
            browser_auth: None,
            options: Vec::new(),
        };
        assert_eq!(definition.auth_option(), None);
    }

    #[test]
    fn every_credential_kind_makes_the_round_trip_it_travels_as() {
        // The strings are the wire, so a kind that did not come back is a client drawing
        // the wrong dialog — or, for a provider with no credential, drawing one at all.
        for kind in [
            CredentialKind::Key,
            CredentialKind::OAuth,
            CredentialKind::External,
            CredentialKind::None,
        ] {
            assert_eq!(CredentialKind::from_wire(kind.as_wire()), Some(kind));
        }
        assert_eq!(CredentialKind::None.as_wire(), "none");
        assert_eq!(CredentialKind::from_wire("quota-frozen"), None);
    }

    #[test]
    fn a_history_point_survives_the_bus() {
        let original = HistoryPoint {
            captured_at: 1_785_700_000,
            used_percent: 37.5,
        };
        let encoded = to_bytes(Context::new_dbus(LE, 0), &original).expect("encodes");
        let (decoded, _): (HistoryPoint, _) = encoded.deserialize().expect("decodes");
        assert_eq!(decoded, original);
    }

    #[test]
    fn a_status_survives_the_bus() {
        let original = status();
        let (decoded, _): (ProviderStatus, _) =
            encode(&original).deserialize().expect("decodes again");
        assert_eq!(decoded, original);
    }

    #[test]
    fn a_window_the_provider_said_nothing_about_carries_no_key_at_all() {
        // The rule this whole encoding exists for: absent must not arrive as zero.
        let encoded = to_bytes(
            Context::new_dbus(LE, 0),
            &WindowStatus::from_window(&window(None)),
        )
        .expect("encodes");
        let (dict, _): (HashMap<String, OwnedValue>, _) = encoded.deserialize().expect("decodes");
        assert!(dict.contains_key("used_percent"));
        assert!(
            !dict.contains_key("resets_at"),
            "a missing reset time must be missing, not zero: {:?}",
            dict.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_client_gets_the_pace_mark_back_rather_than_reimplementing_it() {
        let now = Timestamp::from_unix(1_785_700_000).expect("plausible");
        let published = WindowStatus::from_window(&window(Some(1_785_704_500)));
        let rebuilt = published.to_window();
        assert_eq!(rebuilt.pace(now), window(Some(1_785_704_500)).pace(now));
        assert!(rebuilt.pace(now).is_some());
    }

    #[test]
    fn a_window_with_no_reset_time_has_no_pace_mark_after_a_round_trip() {
        let now = Timestamp::from_unix(1_785_700_000).expect("plausible");
        assert_eq!(
            WindowStatus::from_window(&window(None))
                .to_window()
                .pace(now),
            None
        );
    }

    #[test]
    fn absolutes_survive_the_round_trip_to_the_wire_and_back() {
        let mut source = window(Some(1_785_704_500));
        source.subtitle = Some("100 / 1000 credits".to_owned());
        let published = WindowStatus::from_window(&source);
        assert_eq!(published.subtitle.as_deref(), Some("100 / 1000 credits"));
        assert_eq!(published.to_window().subtitle, source.subtitle);
    }

    #[test]
    fn a_window_with_no_absolutes_publishes_no_subtitle_key() {
        let published = WindowStatus::from_window(&window(None));
        assert!(
            published.subtitle.is_none(),
            "an absent key is how a{{sv}} says the provider did not tell us"
        );
    }

    #[test]
    fn every_state_survives_its_own_string() {
        for state in [
            ProviderState::Pending,
            ProviderState::Ok,
            ProviderState::NoCredential,
            ProviderState::WaitingForKeyring,
            ProviderState::KeyringUnavailable,
            ProviderState::CredentialRejected,
            ProviderState::RateLimited,
            ProviderState::Unreachable,
            ProviderState::Malformed,
        ] {
            assert_eq!(ProviderState::from_wire(state.as_wire()), Some(state));
        }
    }

    #[test]
    fn a_state_from_a_newer_daemon_is_unknown_rather_than_healthy() {
        assert_eq!(ProviderState::from_wire("quota-frozen"), None);
    }

    #[test]
    fn the_three_groups_the_interface_shows() {
        assert_eq!(ProviderState::NoCredential.remedy(), Remedy::YouFixIt);
        assert_eq!(ProviderState::CredentialRejected.remedy(), Remedy::YouFixIt);
        assert_eq!(ProviderState::RateLimited.remedy(), Remedy::ItFixesItself);
        assert_eq!(
            ProviderState::WaitingForKeyring.remedy(),
            Remedy::ItFixesItself,
            "a locked keyring is waited out, not reported as the user's mistake"
        );
        assert_eq!(ProviderState::Malformed.remedy(), Remedy::TheyBrokeIt);
        assert_eq!(ProviderState::Ok.remedy(), Remedy::Nothing);
    }

    #[test]
    fn a_failed_poll_keeps_the_last_good_reading_on_screen() {
        let mut status = status();
        status.set_state(ProviderState::Unreachable, Some("timed out".into()));
        assert_eq!(
            status.windows.len(),
            2,
            "the numbers stay while the state changes"
        );
        assert_eq!(status.captured_at, Some(1_785_700_000));
        assert_eq!(status.state(), Some(ProviderState::Unreachable));
    }

    #[test]
    fn the_card_finds_the_plan_where_the_adapters_file_it() {
        assert_eq!(status().plan(), Some("pro"));
    }

    #[test]
    fn a_provider_that_says_nothing_about_a_plan_leaves_the_line_off() {
        let mut status = status();
        status.details.retain(|s| s.title != DetailSection::PLAN);
        assert_eq!(status.plan(), None, "an absent plan is absent, not empty");
    }

    #[test]
    fn an_account_never_polled_has_no_reading_to_offer() {
        let pending = ProviderStatus::pending(&ProviderId::new("kimi"), &AccountId::default());
        assert!(pending.to_snapshot().is_none());
        assert_eq!(pending.state(), Some(ProviderState::Pending));
    }

    #[test]
    fn a_reading_round_trips_back_into_the_domain() {
        let status = status();
        let snapshot = status.to_snapshot().expect("there is a reading");
        assert_eq!(snapshot.provider.as_str(), "zai");
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(
            snapshot.dominant_window().expect("present").length,
            WindowLength::from_secs(18_000)
        );
    }
    #[test]
    fn data_info_survives_the_bus() {
        let original = DataInfo {
            config_path: "/home/test/.config/tidemark/config.toml".into(),
            history_path: "/home/test/.local/share/tidemark/history.db".into(),
            history_bytes: 8192,
            key_schema: "io.github.zbndev.Tidemark.ProviderKey".into(),
            token_schema: "io.github.zbndev.Tidemark.ProviderToken".into(),
            release_check_available: true,
        };

        let encoded = to_bytes(Context::new_dbus(LE, 0), &original).expect("encodes");
        let (decoded, _): (DataInfo, _) = encoded.deserialize().expect("decodes again");
        assert_eq!(decoded, original);
    }

    #[test]
    fn preferences_survive_the_bus() {
        let original = Preferences {
            release_check: false,
            minimize_on_close: false,
            startup_mode: "daemon".into(),
            history_retention: "one-year".into(),
            proxy_mode: "socks5".into(),
            proxy_host: "127.0.0.1".into(),
            proxy_port: 1080,
        };

        let encoded = to_bytes(Context::new_dbus(LE, 0), &original).expect("encodes");
        let (decoded, _): (Preferences, _) = encoded.deserialize().expect("decodes again");
        assert_eq!(decoded, original);
    }

    #[test]
    fn only_the_four_named_proxy_modes_are_known() {
        for mode in [
            Preferences::PROXY_OFF,
            Preferences::PROXY_HTTP,
            Preferences::PROXY_HTTPS,
            Preferences::PROXY_SOCKS5,
        ] {
            assert!(Preferences::valid_proxy_mode(mode), "{mode}");
        }
        assert!(!Preferences::valid_proxy_mode("socks4"));
        assert!(!Preferences::valid_proxy_mode(""));
    }
}
