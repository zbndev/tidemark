//! The stream against a daemon this test serves itself.

mod fake;

use std::time::Duration;

use tidemark_types::{AccountId, ProviderId, ProviderState, ProviderStatus};

/// How long the pump is allowed to keep streaming before the test reads what it printed.
///
/// `pump` deliberately never returns while its connection lives: its streams belong to the
/// *client* connection, and dropping the served name only makes the daemon leave the bus —
/// which is a `waiting` line, not the end of the stream. So the test bounds it instead of
/// waiting for a return that would mean the session bus itself had gone away.
const STREAMING: Duration = Duration::from_millis(900);

/// Long enough for a subscription to be registered, or a signal to be delivered, on a
/// loopback session bus.
const SETTLE: Duration = Duration::from_millis(200);

#[test]
fn the_stream_opens_with_a_snapshot_and_follows_every_change() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        let mut out = Vec::new();
        let pumping = async {
            let _ = tidemark_cli::watch::pump(
                &proxy,
                tidemark_cli::watch::Sink::Events,
                None,
                &mut out,
            )
            .await;
        };

        let emitting = async {
            let mut changed = ProviderStatus::pending(
                &ProviderId::new("claude".to_owned()),
                &AccountId::default(),
            );
            changed.state = ProviderState::RateLimited.as_wire().to_owned();
            // Give the pump a moment to subscribe before the signal goes out.
            async_io::Timer::after(SETTLE).await;
            fake::emit_change(&server, changed).await;
            async_io::Timer::after(SETTLE).await;
            // The daemon leaving the bus is an owner change, not a closed stream.
            drop(server);
            async_io::Timer::after(SETTLE).await;
        };

        let bounded = futures_lite::future::or(pumping, async {
            async_io::Timer::after(STREAMING).await;
        });
        futures_lite::future::zip(bounded, emitting).await;

        let text = String::from_utf8(out).expect("utf-8");
        let mut lines = text.lines();

        let snapshot: serde_json::Value =
            serde_json::from_str(lines.next().expect("a snapshot line")).expect("json");
        assert_eq!(snapshot["event"], "snapshot");
        assert_eq!(snapshot["accounts"][0]["provider"], "claude");
        assert_eq!(snapshot["accounts"][0]["state"], "ok");

        let change: serde_json::Value =
            serde_json::from_str(lines.next().expect("a change line")).expect("json");
        assert_eq!(change["event"], "changed");
        assert_eq!(change["account"]["state"], "rate-limited");

        // A daemon that went away says so, so a plugin can grey out rather than keep
        // showing numbers nothing is refreshing.
        let waiting: serde_json::Value =
            serde_json::from_str(lines.next().expect("a waiting line")).expect("json");
        assert_eq!(waiting["event"], "waiting");

        // The snapshot came off the wire once; every later line came from a signal.
        assert_eq!(calls.recorded(), vec!["GetStatus".to_owned()], "{text}");
    });
}
