//! A daemon that answers, and records what it was asked.
//!
//! The commands take a proxy, so a test drives the real command bodies against this
//! instead of the machine's own daemon. There is deliberately no environment variable that
//! redirects the bus name: testability comes from the parameter, not from a back door a
//! user could trip over.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tidemark_ipc::DaemonProxy;
use tidemark_types::{
    AccountId, ProviderDefinition, ProviderId, ProviderState, ProviderStatus, ids,
};

#[derive(Debug, Default, Clone)]
pub struct Calls(Arc<Mutex<Vec<String>>>);

impl Calls {
    pub fn record(&self, call: impl Into<String>) {
        self.0.lock().expect("not poisoned").push(call.into());
    }

    pub fn recorded(&self) -> Vec<String> {
        self.0.lock().expect("not poisoned").clone()
    }
}

pub struct FakeDaemon {
    pub calls: Calls,
    pub statuses: Vec<ProviderStatus>,
}

impl FakeDaemon {
    pub fn with_one_account() -> Self {
        let mut status =
            ProviderStatus::pending(&ProviderId::new("claude".to_owned()), &AccountId::default());
        status.state = ProviderState::Ok.as_wire().to_owned();
        Self {
            calls: Calls::default(),
            statuses: vec![status],
        }
    }
}

#[zbus::interface(name = "io.github.zbndev.Tidemark.Daemon1")]
impl FakeDaemon {
    async fn get_status(&self) -> Vec<ProviderStatus> {
        self.calls.record("GetStatus");
        self.statuses.clone()
    }

    async fn list_providers(&self) -> Vec<ProviderDefinition> {
        self.calls.record("ListProviders");
        Vec::new()
    }

    async fn add_provider(&self, provider: &str) {
        self.calls.record(format!("AddProvider({provider})"));
    }

    async fn remove_provider(&self, provider: &str, account: &str) {
        self.calls
            .record(format!("RemoveProvider({provider},{account})"));
    }

    async fn add_account(&self, provider: &str, account: &str) {
        self.calls
            .record(format!("AddAccount({provider},{account})"));
    }

    async fn rename_account(&self, provider: &str, account: &str, new: &str) {
        self.calls
            .record(format!("RenameAccount({provider},{account},{new})"));
    }

    async fn set_order(&self, providers: Vec<String>) {
        self.calls
            .record(format!("SetOrder({})", providers.join(",")));
    }

    async fn set_account_order(&self, provider: &str, accounts: Vec<String>) {
        self.calls.record(format!(
            "SetAccountOrder({provider},{})",
            accounts.join(",")
        ));
    }

    async fn refresh(&self, provider: &str) {
        self.calls.record(format!("Refresh({provider})"));
    }

    async fn set_key(&self, provider: &str, account: &str, key: &str) {
        // The key's *length* is recorded, never the key: a test that printed a secret
        // would teach the next person that printing secrets is fine.
        self.calls.record(format!(
            "SetKey({provider},{account},{} chars)",
            key.chars().count()
        ));
    }

    async fn sign_out(&self, provider: &str, account: &str) {
        self.calls.record(format!("SignOut({provider},{account})"));
    }

    async fn set_option(&self, provider: &str, account: &str, name: &str, value: &str) {
        self.calls
            .record(format!("SetOption({provider},{account},{name},{value})"));
    }

    async fn set_window_notify(&self, provider: &str, account: &str, window: &str, enabled: bool) {
        self.calls.record(format!(
            "SetWindowNotify({provider},{account},{window},{enabled})"
        ));
    }

    async fn set_proxy(&self, mode: &str, host: &str, port: u16) {
        self.calls.record(format!("SetProxy({mode},{host},{port})"));
    }

    #[zbus(signal)]
    async fn provider_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        status: ProviderStatus,
    ) -> zbus::Result<()>;

    #[zbus(property(emits_changed_signal = "false"))]
    async fn version(&self) -> String {
        "0.0.0-test".to_owned()
    }
}

/// One name per served daemon. `cargo test` runs tests in parallel threads on one session
/// bus, and a second service under a name already taken would fail to start.
static SERVED: AtomicU32 = AtomicU32::new(0);

/// Serves one fake daemon under a unique name and returns a proxy pointed at it.
pub async fn serve(daemon: FakeDaemon) -> Option<(zbus::Connection, DaemonProxy<'static>, Calls)> {
    let calls = daemon.calls.clone();
    let client = zbus::Connection::session().await.ok()?;
    let name = format!(
        "io.github.zbndev.TidemarkCliTest{}x{}",
        std::process::id(),
        SERVED.fetch_add(1, Ordering::Relaxed)
    );
    let server = zbus::connection::Builder::session()
        .expect("session builder")
        .name(name.as_str())
        .expect("unique test name")
        .serve_at(ids::OBJECT_PATH, daemon)
        .expect("serves the object")
        .build()
        .await
        .expect("test service starts");
    let proxy = DaemonProxy::builder(&client)
        .destination(name)
        .expect("valid destination")
        .build()
        .await
        .expect("proxy builds");
    Some((server, proxy, calls))
}

/// Emits `ProviderChanged` from the served object, the way the real daemon does.
///
/// This module is compiled into every integration test binary and each one uses the part
/// it needs, so an unused helper here is expected rather than dead — `expect` would itself
/// go unfulfilled in the binary that does use it.
#[allow(dead_code)]
pub async fn emit_change(server: &zbus::Connection, status: ProviderStatus) {
    let emitter =
        zbus::object_server::SignalEmitter::new(server, ids::OBJECT_PATH).expect("valid emitter");
    FakeDaemon::provider_changed(&emitter, status)
        .await
        .expect("signal goes out");
}
