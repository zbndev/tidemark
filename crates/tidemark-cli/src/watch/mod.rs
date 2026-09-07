//! The daemon's signals as a line-oriented stream.
//!
//! One JSON object per line, flushed as it is written, so a plugin can read a pipe instead
//! of polling a snapshot every few seconds. The first line of a connection is always a
//! snapshot, and so is the first line after a reconnect: whatever the daemon published
//! while nothing was listening was announced to nobody, and re-reading is the only honest
//! recovery.

pub mod mirror;

use std::io::Write;
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use serde::Serialize;
use tidemark_ipc::{
    DaemonProxy, DataChanged, OrderChanged, PreferencesChanged, ProviderChanged, ProviderRemoved,
    UpdateChanged,
};
use tidemark_types::{DataInfo, Preferences, ProviderStatus, Timestamp};
use zbus::export::futures_core::Stream;

use crate::exit::{Exit, Failure};
use crate::titles::Titles;
use crate::{connect, format};

/// How long to wait before trying the bus again. Only reached when the *bus* is
/// unreachable; a daemon that is merely not running is waited for by name.
const RETRY: Duration = Duration::from_secs(5);

/// One line of the stream.
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event<'a> {
    Snapshot { accounts: &'a [ProviderStatus] },
    Changed { account: &'a ProviderStatus },
    Removed { provider: &'a str, account: &'a str },
    Order { providers: &'a [String] },
    Preferences { preferences: &'a Preferences },
    Data { data: &'a DataInfo },
    Update { version: &'a str },
    Waiting,
}

/// One event, one line. The published wire types carry their D-Bus signatures under
/// `serde_json`, so the line goes through [`crate::format::payload`] — the same peeling
/// `usage --format json` does, because a plugin reading the stream and a plugin reading a
/// snapshot must parse the same shapes.
pub fn line(event: &Event<'_>) -> Result<String, serde_json::Error> {
    serde_json::to_string(&crate::format::payload(serde_json::to_value(event)?))
}

/// What the stream prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// One JSON event per line.
    Events,
    /// One Waybar card per change, re-rendered from the mirror.
    Waybar,
}

/// Streams until the process is killed. Each connection's events are printed, and the loop
/// re-reads everything after a gap.
pub async fn run(sink: Sink, provider: Option<String>) -> Result<Exit, Failure> {
    let mut out = std::io::stdout();
    loop {
        match connect::daemon().await {
            Ok(proxy) => {
                if let Err(error) = pump(&proxy, sink, provider.as_deref(), &mut out).await {
                    eprintln!("{error}");
                }
            }
            Err(error) => eprintln!("{error}"),
        }
        emit(&mut out, &Event::Waiting)?;
        async_io::Timer::after(RETRY).await;
    }
}

/// One connection's worth of streaming. Returns when a stream ends, which on the session
/// bus means the connection is finished with.
pub async fn pump(
    proxy: &DaemonProxy<'_>,
    sink: Sink,
    provider: Option<&str>,
    out: &mut impl Write,
) -> zbus::Result<()> {
    // Subscribed before the first GetStatus, so a poll finishing between the two arrives as
    // a signal rather than being missed by both.
    let mut owner = pin!(proxy.inner().receive_owner_changed().await?);
    let mut changes = pin!(proxy.receive_provider_changed().await?);
    let mut removals = pin!(proxy.receive_provider_removed().await?);
    let mut orders = pin!(proxy.receive_order_changed().await?);
    let mut updates = pin!(proxy.receive_update_changed().await?);
    let mut preferences = pin!(proxy.receive_preferences_changed().await?);
    let mut data = pin!(proxy.receive_data_changed().await?);

    // A tooltip names providers the way the catalog spells them; the event stream names
    // them by slug and would be paying for this round trip with nothing to show.
    let titles = match sink {
        Sink::Waybar => Titles::index(&proxy.list_providers().await?),
        Sink::Events => Titles::default(),
    };
    let mut statuses = proxy.get_status().await?;
    publish(
        out,
        sink,
        provider,
        &titles,
        &statuses,
        &Event::Snapshot {
            accounts: &statuses,
        },
    )?;

    loop {
        let event = std::future::poll_fn(|context| {
            if let Poll::Ready(owner) = owner.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Owner(owner));
            }
            if let Poll::Ready(change) = changes.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Changed(change));
            }
            if let Poll::Ready(removal) = removals.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Removed(removal));
            }
            if let Poll::Ready(order) = orders.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Order(order));
            }
            if let Poll::Ready(update) = updates.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Update(update));
            }
            if let Poll::Ready(changed) = preferences.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Preferences(changed));
            }
            if let Poll::Ready(changed) = data.as_mut().poll_next(context) {
                return Poll::Ready(Signal::Data(changed));
            }
            Poll::Pending
        })
        .await;

        match event {
            // The daemon appeared, or a newer one replaced it. Re-read: what it published
            // while nothing was listening was announced to nobody.
            Signal::Owner(Some(Some(_))) => {
                statuses = proxy.get_status().await?;
                publish(
                    out,
                    sink,
                    provider,
                    &titles,
                    &statuses,
                    &Event::Snapshot {
                        accounts: &statuses,
                    },
                )?;
            }
            Signal::Owner(Some(None)) => {
                publish(out, sink, provider, &titles, &statuses, &Event::Waiting)?
            }
            Signal::Changed(Some(signal)) => {
                let status = signal.args()?.status;
                publish(
                    out,
                    sink,
                    provider,
                    &titles,
                    &statuses,
                    &Event::Changed { account: &status },
                )?;
                mirror::apply(&mut statuses, mirror::Change::Upsert(status));
                republish(out, sink, provider, &titles, &statuses)?;
            }
            Signal::Removed(Some(signal)) => {
                let args = signal.args()?;
                publish(
                    out,
                    sink,
                    provider,
                    &titles,
                    &statuses,
                    &Event::Removed {
                        provider: args.provider,
                        account: args.account,
                    },
                )?;
                mirror::apply(
                    &mut statuses,
                    mirror::Change::Remove {
                        provider: args.provider.to_owned(),
                        account: args.account.to_owned(),
                    },
                );
                republish(out, sink, provider, &titles, &statuses)?;
            }
            Signal::Order(Some(signal)) => {
                let providers = signal.args()?.providers;
                publish(
                    out,
                    sink,
                    provider,
                    &titles,
                    &statuses,
                    &Event::Order {
                        providers: &providers,
                    },
                )?;
                mirror::apply(&mut statuses, mirror::Change::Order(providers));
                republish(out, sink, provider, &titles, &statuses)?;
            }
            Signal::Update(Some(signal)) => publish(
                out,
                sink,
                provider,
                &titles,
                &statuses,
                &Event::Update {
                    version: signal.args()?.version,
                },
            )?,
            Signal::Preferences(Some(signal)) => {
                let args = signal.args()?;
                publish(
                    out,
                    sink,
                    provider,
                    &titles,
                    &statuses,
                    &Event::Preferences {
                        preferences: &args.preferences,
                    },
                )?;
            }
            Signal::Data(Some(signal)) => {
                let args = signal.args()?;
                publish(
                    out,
                    sink,
                    provider,
                    &titles,
                    &statuses,
                    &Event::Data { data: &args.data },
                )?;
            }
            // Any stream ending means this connection is done.
            Signal::Owner(None)
            | Signal::Changed(None)
            | Signal::Removed(None)
            | Signal::Order(None)
            | Signal::Update(None)
            | Signal::Preferences(None)
            | Signal::Data(None) => return Ok(()),
        }
    }
}

/// Which stream produced something.
enum Signal {
    Owner(Option<Option<zbus::names::UniqueName<'static>>>),
    Changed(Option<ProviderChanged>),
    Removed(Option<ProviderRemoved>),
    Order(Option<OrderChanged>),
    Update(Option<UpdateChanged>),
    Preferences(Option<PreferencesChanged>),
    Data(Option<DataChanged>),
}

/// In event mode, one line per event; in Waybar mode, events are silent and only the card
/// is printed, from `republish`.
fn publish(
    out: &mut impl Write,
    sink: Sink,
    provider: Option<&str>,
    titles: &Titles,
    statuses: &[ProviderStatus],
    event: &Event<'_>,
) -> Result<(), std::io::Error> {
    match sink {
        Sink::Events => emit(out, event),
        Sink::Waybar => match event {
            Event::Snapshot { .. } | Event::Waiting => card(out, provider, titles, statuses),
            _ => Ok(()),
        },
    }
}

/// After a change has been applied to the mirror, a Waybar module wants the whole card
/// again: its text is the worst window across every account, not the one that changed.
fn republish(
    out: &mut impl Write,
    sink: Sink,
    provider: Option<&str>,
    titles: &Titles,
    statuses: &[ProviderStatus],
) -> Result<(), std::io::Error> {
    match sink {
        Sink::Events => Ok(()),
        Sink::Waybar => card(out, provider, titles, statuses),
    }
}

fn card(
    out: &mut impl Write,
    provider: Option<&str>,
    titles: &Titles,
    statuses: &[ProviderStatus],
) -> Result<(), std::io::Error> {
    let selected = format::select(statuses, provider, None);
    let rendered = format::waybar::render(&selected, titles, Timestamp::now())
        .map_err(std::io::Error::other)?;
    writeln!(out, "{rendered}")?;
    out.flush()
}

/// Written and flushed per line: stdout is block-buffered when it is a pipe, and a plugin
/// reading a pipe must not wait for a buffer to fill.
fn emit(out: &mut impl Write, event: &Event<'_>) -> Result<(), std::io::Error> {
    writeln!(out, "{}", line(event).map_err(std::io::Error::other)?)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId};

    #[test]
    fn a_snapshot_names_itself_and_carries_every_account() {
        let accounts = vec![ProviderStatus::pending(
            &ProviderId::new("zai".to_owned()),
            &AccountId::default(),
        )];
        let text = line(&Event::Snapshot {
            accounts: &accounts,
        })
        .expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["event"], "snapshot");
        assert_eq!(value["accounts"][0]["provider"], "zai");
        assert!(!text.contains('\n'), "one event is one line: {text}");
    }

    #[test]
    fn a_removal_names_the_account_that_went_away() {
        let text = line(&Event::Removed {
            provider: "claude",
            account: "work",
        })
        .expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["event"], "removed");
        assert_eq!(value["provider"], "claude");
        assert_eq!(value["account"], "work");
    }

    #[test]
    fn waiting_is_a_line_of_its_own() {
        let text = line(&Event::Waiting).expect("serializes");
        assert_eq!(text, r#"{"event":"waiting"}"#);
    }
}
