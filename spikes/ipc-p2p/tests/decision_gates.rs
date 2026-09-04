use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::StreamExt as _;
use tidemark_ipc_p2p_spike::{
    PEER_QUEUE_BOUND, RunningServer, bounded, connect, expected_contract, no_signal_is_ready,
    observed_signal, pending_status, read_signals,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct TestRoot(PathBuf);

impl TestRoot {
    fn new(name: &str) -> Self {
        let serial = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA is set"))
            .join("tidemark-ipc-p2p-tests")
            .join(format!("{}-{name}-{serial}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("old test root is removable");
        }
        fs::create_dir_all(&root).expect("test root can be created");
        Self(root)
    }

    fn endpoint(&self) -> PathBuf {
        self.0.join("run").join("d.sock")
    }

    fn cleanup(self) {
        fs::remove_dir_all(&self.0).expect("test root is removable after server shutdown");
        // The shared parent is left empty by the leaf removal above; remove it too.
        let _ = fs::remove_dir(self.0.parent().expect("test root has a parent"));
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
        let _ = fs::remove_dir(self.0.parent().expect("test root has a parent"));
    }
}

async fn client_with_signals(
    endpoint: &Path,
    label: &str,
    hold_delivery: bool,
) -> (
    tidemark_ipc_p2p_spike::Client,
    zbus::proxy::SignalStream<'static>,
) {
    let client = bounded("client connection", connect(endpoint))
        .await
        .expect("connection guard")
        .expect("client connects");
    let signals = bounded("proxy signal subscription", client.signals())
        .await
        .expect("subscription guard")
        .expect("client subscribes");
    bounded("client registration", client.hello(label, hold_delivery))
        .await
        .expect("registration guard")
        .expect("client registers");
    (client, signals)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_5_two_real_proxies_receive_methods_and_all_six_signals_once_in_order() {
    let root = TestRoot::new("gate-5");
    let endpoint = root.endpoint();
    let server = RunningServer::start(&endpoint, "5.19-a1", vec![pending_status("zai")])
        .expect("AF_UNIX server starts");
    let handle = server.handle();

    let (first, mut first_signals) = client_with_signals(&endpoint, "first", false).await;
    let (second, mut second_signals) = client_with_signals(&endpoint, "second", false).await;

    for client in [&first, &second] {
        assert_eq!(
            bounded("Version", client.proxy().version())
                .await
                .expect("Version guard")
                .expect("Version answers"),
            "5.19-a1"
        );
        let statuses = bounded("GetStatus", client.proxy().get_status())
            .await
            .expect("GetStatus guard")
            .expect("GetStatus answers");
        assert_eq!(statuses, vec![pending_status("zai")]);
    }

    bounded("six-signal publication", handle.publish_contract(5))
        .await
        .expect("publication guard")
        .expect("all peers accept all signals");
    let first_received = read_signals(&mut first_signals, 6)
        .await
        .expect("first proxy reads six signals");
    let second_received = read_signals(&mut second_signals, 6)
        .await
        .expect("second proxy reads six signals");
    assert_eq!(first_received, expected_contract(5));
    assert_eq!(second_received, expected_contract(5));

    assert_eq!(first.proxy().fence().await.expect("first fence"), 6);
    assert_eq!(second.proxy().fence().await.expect("second fence"), 6);
    assert!(
        no_signal_is_ready(&mut first_signals),
        "first proxy received a duplicate seventh signal"
    );
    assert!(
        no_signal_is_ready(&mut second_signals),
        "second proxy received a duplicate seventh signal"
    );
    assert_eq!(
        handle.delivery_counts().into_values().collect::<Vec<_>>(),
        vec![6, 6]
    );

    drop(first_signals);
    drop(second_signals);
    drop(first);
    drop(second);
    bounded("gate 5 server shutdown", server.shutdown())
        .await
        .expect("shutdown guard")
        .expect("server stops cleanly");
    root.cleanup();
    println!(
        "GATE 5 PASS: 2 proxies; Version+GetStatus; 6/6 ordered signals per client; 0 duplicates"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_6_subscribing_before_get_status_closes_the_update_race() {
    let root = TestRoot::new("gate-6");
    let endpoint = root.endpoint();
    let old = pending_status("zai");
    let server = RunningServer::start(&endpoint, "race-a1", vec![old.clone()])
        .expect("AF_UNIX server starts");
    let handle = server.handle();
    let (client, mut signals) = client_with_signals(&endpoint, "race-client", false).await;
    let mut race = handle.arm_get_status_race().await.expect("race hook arms");
    let mut changed = pending_status("zai");
    changed.captured_at = Some(1_800_000_006);

    let get_status = bounded("blocked GetStatus", client.proxy().get_status());
    let update_during_call = async {
        race.wait_until_entered().await?;
        handle.replace_status_and_publish(changed.clone()).await?;
        race.release()
    };
    let (snapshot, race_result) = tokio::join!(get_status, update_during_call);
    race_result.expect("exact race sequence completes");
    assert_eq!(
        snapshot
            .expect("GetStatus guard")
            .expect("GetStatus answers"),
        vec![old],
        "the method deliberately returns the snapshot from before the concurrent commit"
    );

    let message = bounded("racing ProviderChanged", signals.next())
        .await
        .expect("signal guard")
        .expect("signal stream remains open");
    assert_eq!(
        observed_signal(&message).expect("signal uses a real wire value"),
        tidemark_ipc_p2p_spike::ObservedSignal::ProviderChanged(changed)
    );
    assert_eq!(client.proxy().fence().await.expect("ordering fence"), 1);
    assert!(no_signal_is_ready(&mut signals));

    drop(signals);
    drop(client);
    bounded("gate 6 server shutdown", server.shutdown())
        .await
        .expect("shutdown guard")
        .expect("server stops cleanly");
    root.cleanup();
    println!(
        "GATE 6 PASS: subscription existed before blocked GetStatus; concurrent ProviderChanged was not missed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_7_the_129th_queued_announcement_evicts_a_lagging_peer() {
    let root = TestRoot::new("gate-7");
    let endpoint = root.endpoint();
    let server = RunningServer::start(&endpoint, "lag-a1", vec![pending_status("zai")])
        .expect("AF_UNIX server starts");
    let handle = server.handle();
    let (laggard, _signals) = client_with_signals(&endpoint, "laggard", true).await;

    for sequence in 0..PEER_QUEUE_BOUND as u32 {
        let outcome = handle
            .publish_unacknowledged_update(sequence)
            .await
            .expect("bounded try_send succeeds");
        assert_eq!(outcome.accepted, 1, "announcement {sequence}");
        assert!(outcome.evicted.is_empty(), "announcement {sequence}");
    }
    assert_eq!(handle.queue_remaining("laggard").expect("peer exists"), 0);
    assert_eq!(handle.peer_count(), 1, "128 entries fit exactly");

    let overflow = handle
        .publish_unacknowledged_update(PEER_QUEUE_BOUND as u32)
        .await
        .expect("overflow is a classified outcome");
    assert_eq!(overflow.accepted, 0);
    assert_eq!(overflow.evicted, vec!["laggard"]);
    assert_eq!(handle.peer_count(), 0);
    bounded("laggard connection eviction", laggard.connection().closed())
        .await
        .expect("laggard closes at the bound");

    drop(laggard);
    bounded("gate 7 server shutdown", server.shutdown())
        .await
        .expect("shutdown guard")
        .expect("server stops cleanly");
    root.cleanup();
    println!(
        "GATE 7 PASS: queue accepted 128 entries; entry 129 evicted and closed the whole peer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_8_a_client_recovers_with_a_full_reload_after_server_restart() {
    let root = TestRoot::new("gate-8");
    let endpoint = root.endpoint();
    let first_server =
        RunningServer::start(&endpoint, "before-restart", vec![pending_status("zai")])
            .expect("first AF_UNIX server starts");
    let first = bounded("first client connection", connect(&endpoint))
        .await
        .expect("connection guard")
        .expect("first client connects");
    first
        .hello("before-restart-client", false)
        .await
        .expect("first client registers");
    assert_eq!(
        first.proxy().version().await.expect("first Version"),
        "before-restart"
    );
    assert_eq!(
        first.proxy().get_status().await.expect("first GetStatus"),
        vec![pending_status("zai")]
    );

    bounded("first server shutdown", first_server.shutdown())
        .await
        .expect("shutdown guard")
        .expect("first server stops");
    bounded("old client EOF", first.connection().closed())
        .await
        .expect("old connection observes EOF");
    assert!(first.connection().is_closed());

    let second_server =
        RunningServer::start(&endpoint, "after-restart", vec![pending_status("claude")])
            .expect("replacement AF_UNIX server starts on the same path");
    let second = bounded("replacement client connection", connect(&endpoint))
        .await
        .expect("connection guard")
        .expect("replacement client connects");
    let mut signals = second
        .signals()
        .await
        .expect("replacement subscribes first");
    second
        .hello("after-restart-client", false)
        .await
        .expect("replacement client registers");
    assert_eq!(
        second.proxy().version().await.expect("replacement Version"),
        "after-restart"
    );
    assert_eq!(
        second.proxy().get_status().await.expect("full reload"),
        vec![pending_status("claude")]
    );
    second_server
        .handle()
        .replace_status_and_publish(pending_status("codex"))
        .await
        .expect("replacement server publishes");
    let message = bounded("post-restart signal", signals.next())
        .await
        .expect("signal guard")
        .expect("replacement stream remains open");
    assert_eq!(
        observed_signal(&message).expect("post-restart wire value"),
        tidemark_ipc_p2p_spike::ObservedSignal::ProviderChanged(pending_status("codex"))
    );

    drop(first);
    drop(signals);
    drop(second);
    bounded("replacement server shutdown", second_server.shutdown())
        .await
        .expect("shutdown guard")
        .expect("replacement server stops cleanly");
    root.cleanup();
    println!(
        "GATE 8 PASS: EOF observed; same endpoint rebound; Version+full GetStatus reload+signal recovered"
    );
}
