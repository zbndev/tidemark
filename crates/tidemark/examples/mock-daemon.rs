//! A stand-in for `tidemarkd`, for looking at the interface with more than one account.
//!
//! Tidemark shows one provider per configured account, and this machine has one. The layout
//! decisions the grid actually has to get right — how three columns collapse to one, what a
//! row of cards of different heights does, whether a chip is legible next to a headline
//! number — cannot be judged from a single card, and `CONTEXT.md` says they cannot be judged
//! from a mockup either.
//!
//! So this serves the real interface, on the real bus name, with invented readings that
//! cover the cases that are awkward to produce on demand: a window with no reset time, a
//! nearly exhausted one, a failure with a last good reading underneath it, and an account
//! that has never answered at all.
//!
//! Three of them are here because a card's size must not depend on them: an error message
//! longer than a card and unbreakable by word wrapping, a window whose title and absolutes
//! are longer than the room they have, and a state string this build does not know and shows
//! verbatim. All three arrive from another process, and each one of them widened every card
//! on screen once.
//!
//! ```sh
//! systemctl --user stop tidemarkd        # it owns the name; only one process can
//! cargo run -p tidemark --example mock-daemon
//! cargo run -p tidemark                  # in another terminal
//! ```
//!
//! It is a development aid, not a test fixture: nothing in the suite depends on it.

use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ExternalLogin, OptionChoice,
    ProviderDefinition, ProviderId, ProviderOption, ProviderState, ProviderStatus, Snapshot,
    Timestamp, Window, WindowKey, WindowLength, ids, provider_label,
};
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface};

struct MockDaemon {
    statuses: Vec<ProviderStatus>,
}

#[interface(name = "io.github.zbndev.Tidemark.Daemon1")]
impl MockDaemon {
    // The window asks for the catalog before it asks for readings, and shows "the daemon is
    // not running" if either fails — so a mock that answered only `GetStatus` drew no cards
    // at all. The entries are the accounts served below, with the least metadata the
    // settings panes will accept; this mock exists to be looked at, not to be configured.
    //
    // The exception is the credential choice. Three of the invented accounts have two
    // credentials, the pane that picks between them is two screens rather than one control,
    // and there is no way to look at the second screen on a machine whose real daemon
    // reports the same answer every time. So the external logins are described here in the
    // shape the real catalog publishes them in.
    async fn list_providers(&self) -> Vec<ProviderDefinition> {
        self.statuses
            .iter()
            .map(|status| {
                let external = external_login(&status.provider);
                ProviderDefinition {
                    provider: status.provider.clone(),
                    title: provider_label(&status.provider).to_owned(),
                    // The invented accounts include the three OAuth providers; a key field
                    // offered for those would mislead whoever is looking at the panes.
                    credential: if external.is_some() {
                        CredentialKind::OAuth
                    } else {
                        CredentialKind::Key
                    }
                    .as_wire()
                    .to_owned(),
                    credential_hint: "Paste a key.".to_owned(),
                    options: external.iter().map(source_option).collect(),
                    external,
                    browser_auth: None,
                }
            })
            .collect()
    }

    async fn get_status(&self) -> Vec<ProviderStatus> {
        self.statuses.clone()
    }

    async fn get_update(&self) -> String {
        "0.2.0".into()
    }

    async fn refresh(&self, provider: &str) -> fdo::Result<()> {
        println!("refresh({provider:?})");
        Ok(())
    }

    /// The credential choice, answered the way the daemon answers it: the setting is
    /// written, and the account is republished on the half it now sits on.
    async fn set_option(
        &mut self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        provider: &str,
        account: &str,
        name: &str,
        value: &str,
    ) -> fdo::Result<()> {
        println!("set_option({provider:?}, {account:?}, {name:?}, {value:?})");
        let Some(status) = self
            .statuses
            .iter_mut()
            .find(|status| status.provider == provider && status.account == account)
        else {
            return Err(fdo::Error::InvalidArgs(format!("no {provider}/{account}")));
        };
        if name != AUTH_SOURCE {
            return Err(fdo::Error::InvalidArgs(format!("no setting {name}")));
        }
        status.auth_source = Some(value.to_owned());
        let published = status.clone();
        Self::provider_changed(&emitter, published).await?;
        Ok(())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    #[zbus(signal)]
    pub async fn provider_changed(
        emitter: &SignalEmitter<'_>,
        status: ProviderStatus,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn update_changed(emitter: &SignalEmitter<'_>, version: &str) -> zbus::Result<()>;
}

/// The setting the credential pill writes, spelled as the real catalog spells it.
const AUTH_SOURCE: &str = "source";

/// The three accounts that have a second credential, described as the daemon describes
/// them: one that Tidemark refreshes in place, and one it only reads.
fn external_login(provider: &str) -> Option<ExternalLogin> {
    let (label, location, command, writes_back) = match provider {
        "antigravity" => (
            "agy session",
            "a signed-in agy server on this machine",
            "agy",
            false,
        ),
        "claude" => (
            "Claude Code login",
            "~/.claude/.credentials.json",
            "claude",
            true,
        ),
        "codex" => ("Codex CLI login", "~/.codex/auth.json", "codex login", true),
        _ => return None,
    };
    Some(ExternalLogin {
        option: AUTH_SOURCE.to_owned(),
        label: label.to_owned(),
        location: location.to_owned(),
        command: command.to_owned(),
        writes_back,
    })
}

/// The choice itself: two values, named after the two credentials.
fn source_option(external: &ExternalLogin) -> ProviderOption {
    ProviderOption {
        name: external.option.clone(),
        title: "Credential".to_owned(),
        description: None,
        value: "auto".to_owned(),
        choices: vec![
            OptionChoice {
                value: "oauth".to_owned(),
                title: "Tidemark login".to_owned(),
            },
            OptionChoice {
                value: "cli".to_owned(),
                title: external.label.clone(),
            },
        ],
    }
}

fn window(title: &str, length: u64, used: f64, resets_in: Option<i64>) -> Window {
    let now = Timestamp::now();
    Window {
        key: WindowKey::for_length(WindowLength::from_secs(length).expect("nonzero")),
        title: title.to_owned(),
        subtitle: None,
        used_percent: used,
        resets_at: resets_in.map(|secs| now.saturating_add_seconds(secs)),
        length: WindowLength::from_secs(length),
    }
}

fn account(provider: &str, plan: &str, windows: Vec<Window>) -> ProviderStatus {
    let provider = ProviderId::new(provider);
    let account = AccountId::default();
    let mut status = ProviderStatus::pending(&provider, &account);
    status.set_reading(&Snapshot {
        provider,
        account,
        captured_at: Timestamp::now().saturating_add_seconds(-90),
        windows,
        details: vec![DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Level".to_owned(),
                value: plan.to_owned(),
            }],
        }],
    });
    status.next_poll_at = Some(Timestamp::now().as_unix() + 210);
    status
}

fn statuses() -> Vec<ProviderStatus> {
    let mut claude = account(
        "claude",
        "max",
        vec![
            // The case the bar is designed around: a window the provider gave no reset
            // time for, so there is no pace mark to draw.
            window("5 hours", 18_000, 12.0, None),
            window("1 week", 604_800, 61.0, Some(3 * 86_400)),
            window("1 week (Opus)", 604_800, 4.0, Some(3 * 86_400)),
        ],
    );
    claude.next_poll_at = Some(Timestamp::now().as_unix() + 40);
    // Reading Claude Code's own file, which is there and which Tidemark refreshes in
    // place: the case the write-back sentence exists for.
    claude.auth_source = Some("cli".to_owned());
    claude.external_present = Some(true);
    claude.has_credential = Some(false);

    let mut codex = account(
        "codex",
        "plus",
        // One window and nothing else, which is the live shape of this account.
        vec![window("1 week", 604_800, 97.5, Some(29 * 3600))],
    );
    // Signed in here, with a CLI login on the machine that is deliberately not being used.
    codex.auth_source = Some("oauth".to_owned());
    codex.external_present = Some(true);
    codex.has_credential = Some(true);
    // A state this build has never heard of, which the card shows verbatim because the
    // daemon is the one that can name it. Long on purpose: the title row has a mark, a name
    // and a plan on it already, and the chip is the part that gives way.
    codex.state = "waiting-for-billing-cycle".to_owned();

    let mut kimi = account(
        "kimi",
        "starter",
        vec![
            window("3 hours", 10_800, 84.0, Some(1_800)),
            window("1 day", 86_400, 33.0, Some(20 * 3600)),
        ],
    );
    // A failure with a reading underneath it: the numbers stay, the chip changes.
    kimi.set_state(
        ProviderState::RateLimited,
        Some("Asked to wait 20 minutes.".into()),
    );

    // The case that used to widen every card on screen, in the words the keyring actually
    // used: a message longer than a card, with a D-Bus error name in it that word wrapping
    // cannot break. The card is meant to wrap it at any character and keep its size.
    let mut antigravity =
        ProviderStatus::pending(&ProviderId::new("antigravity"), &AccountId::default());
    antigravity.set_state(
        ProviderState::KeyringUnavailable,
        Some(
            "the keyring is unavailable: service error org.freedesktop.zbus.Error: i/o error"
                .into(),
        ),
    );
    // The empty half of the CLI screen: nothing signed in either way, so the pane has a
    // command to run and nothing found.
    antigravity.auth_source = Some("cli".to_owned());
    antigravity.external_present = Some(false);
    antigravity.has_credential = Some(false);

    // The account that reports a fixed balance rather than only a percentage: the absolutes
    // behind the 41% are drawn under the bar, in the provider's own words. No live provider
    // fills this in yet, so the mock is the only place the case can be looked at.
    //
    // Both strings are longer than the card is wide, deliberately: the provider chooses how
    // it phrases a window and its absolutes, and neither may choose how wide a card is.
    let mut zai_five_hour = window("5 hours (Coding Plan)", 18_000, 41.0, Some(9_000));
    zai_five_hour.subtitle =
        Some("410 of 1,000 prompts and 2,450,000 of 5,000,000 tokens".to_owned());

    let zai = account(
        "zai",
        "pro",
        vec![
            zai_five_hour,
            window("1 week", 604_800, 22.0, Some(5 * 86_400)),
            window("MCP", 2_592_000, 3.0, Some(19 * 86_400)),
        ],
    );

    vec![claude, codex, kimi, antigravity, zai]
}

fn main() -> gtk::glib::ExitCode {
    // glib's main loop rather than a runtime of its own: this crate has no async runtime
    // and does not want one, and zbus is happy to be driven by whatever is running.
    let looper = gtk::glib::MainLoop::new(None, false);

    gtk::glib::spawn_future_local(async move {
        let statuses = statuses();
        println!(
            "serving {} accounts on {}",
            statuses.len(),
            ids::DAEMON_BUS_NAME
        );
        let connection = zbus::connection::Builder::session()
            .and_then(|builder| builder.name(ids::DAEMON_BUS_NAME))
            .and_then(|builder| builder.serve_at(ids::OBJECT_PATH, MockDaemon { statuses }))
            .expect("a session bus and a free name")
            .build()
            .await;
        match connection {
            Ok(connection) => {
                // Held for as long as this future lives, which is for as long as the
                // process does; dropping it would take the name off the bus.
                let _connection = connection;
                std::future::pending::<()>().await;
            }
            Err(error) => {
                eprintln!("cannot serve: {error}");
                eprintln!("stop the real daemon first: systemctl --user stop tidemarkd");
                std::process::exit(1);
            }
        }
    });

    looper.run();
    gtk::glib::ExitCode::SUCCESS
}
