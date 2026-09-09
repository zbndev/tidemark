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
    AccountId, PluginInfo, Presentation, ProviderDefinition, ProviderId, ProviderState,
    ProviderStatus, ids,
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
    /// What every plugin method refuses with, for the tests about a refusal reaching the
    /// user. `None` is a daemon that accepts everything.
    pub refusal: Option<String>,
    /// What the catalog says about installed plugins, which is where `plugin list` reads
    /// them from: the daemon publishes one catalog, not a second plugin-only listing.
    pub definitions: Vec<ProviderDefinition>,
}

impl FakeDaemon {
    pub fn with_one_account() -> Self {
        let mut status =
            ProviderStatus::pending(&ProviderId::new("claude".to_owned()), &AccountId::default());
        status.state = ProviderState::Ok.as_wire().to_owned();
        Self {
            calls: Calls::default(),
            statuses: vec![status],
            refusal: None,
            definitions: Vec::new(),
        }
    }

    /// A daemon carrying one installed plugin in its catalog.
    ///
    /// Unused in the binaries that do not test plugins — see `emit_change` for why that is
    /// expected here rather than dead.
    #[allow(dead_code)]
    pub fn with_one_plugin() -> Self {
        Self {
            definitions: vec![
                plugin_definition("com.acme.quota", "Acme AI"),
                ProviderDefinition {
                    provider: "claude".to_owned(),
                    title: "Claude".to_owned(),
                    credential: "oauth".to_owned(),
                    credential_hint: String::new(),
                    external: None,
                    browser_auth: None,
                    options: Vec::new(),
                    plugin: None,
                },
            ],
            ..Self::with_one_account()
        }
    }

    /// A daemon that refuses every plugin method with one message.
    #[allow(dead_code)]
    pub fn refusing(message: &str) -> Self {
        Self {
            refusal: Some(message.to_owned()),
            ..Self::with_one_account()
        }
    }

    /// The refusal as the bus carries it, or nothing when this daemon accepts.
    fn refuse<T>(&self) -> Option<zbus::fdo::Result<T>> {
        self.refusal
            .clone()
            .map(|message| Err(zbus::fdo::Error::InvalidArgs(message)))
    }
}

/// What the daemon publishes for an installed plugin: a catalog entry carrying `plugin`.
#[allow(dead_code)]
pub fn plugin_definition(id: &str, name: &str) -> ProviderDefinition {
    ProviderDefinition {
        provider: id.to_owned(),
        title: name.to_owned(),
        credential: "key".to_owned(),
        credential_hint: "X-Acme-Key".to_owned(),
        external: None,
        browser_auth: None,
        options: Vec::new(),
        plugin: Some(plugin_info(id, name)),
    }
}

/// The metadata every plugin method here answers with.
pub fn plugin_info(id: &str, name: &str) -> PluginInfo {
    PluginInfo {
        id: id.to_owned(),
        name: name.to_owned(),
        plugin_version: "1.2.0".to_owned(),
        method: "GET".to_owned(),
        api_key_header: "X-Acme-Key".to_owned(),
        api_key_prefix: String::new(),
        has_mark: true,
        mark_svg: None,
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
        self.definitions.clone()
    }

    // The plugin surface. The bytes are recorded by *length* rather than content: what
    // these tests are about is that the file the user named reached the daemon whole, and
    // a fixture pasted into an assertion would only be re-asserting the fixture.
    async fn inspect_plugin(&self, bytes: Vec<u8>) -> zbus::fdo::Result<PluginInfo> {
        self.calls
            .record(format!("InspectPlugin({} bytes)", bytes.len()));
        self.refuse()
            .unwrap_or_else(|| Ok(plugin_info("com.acme.quota", "Acme AI")))
    }

    async fn install_plugin(&self, bytes: Vec<u8>) -> zbus::fdo::Result<PluginInfo> {
        self.calls
            .record(format!("InstallPlugin({} bytes)", bytes.len()));
        self.refuse()
            .unwrap_or_else(|| Ok(plugin_info("com.acme.quota", "Acme AI")))
    }

    async fn remove_plugin(&self, provider: &str) -> zbus::fdo::Result<()> {
        self.calls.record(format!("RemovePlugin({provider})"));
        self.refuse().unwrap_or(Ok(()))
    }

    async fn set_plugin_endpoint(
        &self,
        provider: &str,
        account: &str,
        endpoint: &str,
        allow_insecure_http: bool,
    ) -> zbus::fdo::Result<()> {
        self.calls.record(format!(
            "SetPluginEndpoint({provider},{account},{endpoint},{allow_insecure_http})"
        ));
        self.refuse().unwrap_or(Ok(()))
    }

    async fn render_plugin(
        &self,
        bytes: Vec<u8>,
        response: Vec<u8>,
    ) -> zbus::fdo::Result<Presentation> {
        self.calls.record(format!(
            "RenderPlugin({} bytes,{} bytes)",
            bytes.len(),
            response.len()
        ));
        self.refuse().unwrap_or_else(|| {
            Ok(Presentation {
                metrics: vec![tidemark_types::Metric {
                    id: "monthly".to_owned(),
                    title: "Monthly".to_owned(),
                    subtitle: None,
                    value: Some(1.0),
                    maximum: Some(4.0),
                    remaining: Some(3.0),
                    used_percent: Some(25.0),
                    text: None,
                    unit: None,
                    window: None,
                }],
                card: vec![tidemark_types::Widget::gauge(
                    "monthly",
                    tidemark_types::Field::UsedPercent,
                )],
                details: Vec::new(),
            })
        })
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

    async fn set_refresh_mode(&self, mode: &str) {
        self.calls.record(format!("SetRefreshMode({mode})"));
    }

    async fn set_refresh_minutes(&self, minutes: u32) {
        self.calls.record(format!("SetRefreshMinutes({minutes})"));
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
