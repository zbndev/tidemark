//! The D-Bus contract between `tidemarkd` and everything that reads it.
//!
//! One definition, shared: the GTK window and `tidemarkctl` generate their proxy from this
//! trait, so a method that changed shape is a compile error here rather than a runtime
//! error on somebody's machine. That is the same argument that keeps the wire vocabulary in
//! `tidemark-types` — a rule the build enforces beats a rule a hurry can skip.
//!
//! Nothing here opens a connection or decides a policy. Reconnection, retry and the
//! Windows peer-to-peer transport belong to the client that needs them.

use tidemark_types::{
    AuthCandidate, AuthSelection, DataInfo, HistoryPoint, PluginInfo, Preferences, Presentation,
    ProviderDefinition, ProviderStatus,
};

#[zbus::proxy(
    interface = "io.github.zbndev.Tidemark.Daemon1",
    default_service = "io.github.zbndev.Tidemark.Daemon",
    default_path = "/io/github/zbndev/Tidemark"
)]
pub trait Daemon {
    /// Every provider this build knows how to configure.
    fn list_providers(&self) -> zbus::Result<Vec<ProviderDefinition>>;

    /// Adds a compiled-in provider's default account.
    fn add_provider(&self, provider: &str) -> zbus::Result<()>;

    /// Removes one configured account.
    fn remove_provider(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// Adds one more account to a provider the config already has.
    fn add_account(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// Renames one configured account, carrying its credential and history to the new id.
    fn rename_account(&self, provider: &str, account: &str, new: &str) -> zbus::Result<()>;

    /// Every account the daemon watches.
    fn get_status(&self) -> zbus::Result<Vec<ProviderStatus>>;

    /// A newer published application release, or an empty string when none is known.
    fn get_update(&self) -> zbus::Result<String>;
    /// Application-wide preferences stored by the daemon.
    fn get_preferences(&self) -> zbus::Result<Preferences>;

    /// Paths and storage facts for the Preferences data page.
    fn get_data_info(&self) -> zbus::Result<DataInfo>;

    /// Stored points in the current segment of one window, oldest first.
    fn current_segment(
        &self,
        provider: &str,
        account: &str,
        window: &str,
    ) -> zbus::Result<Vec<HistoryPoint>>;

    /// Polls now: one provider by slug, or everything when given an empty string.
    fn refresh(&self, provider: &str) -> zbus::Result<()>;

    /// Asks the running window to come forward; the caller exits after this.
    fn request_activate(&self) -> zbus::Result<()>;

    /// Stores an API key for an account.
    fn set_key(&self, provider: &str, account: &str, key: &str) -> zbus::Result<()>;

    /// Stores a browser session the user pasted in, and puts the account on it.
    fn set_session(&self, provider: &str, account: &str, session: &str) -> zbus::Result<()>;

    /// Removes whatever credential Tidemark holds for an account.
    fn sign_out(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// Starts a login and returns the URL to open. Nothing is waited for yet.
    fn begin_login(&self, provider: &str, account: &str) -> zbus::Result<String>;

    /// Waits for a started login to finish. Long-running: up to the browser timeout.
    fn await_login(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// Abandons a login in progress. Not an error when there is none.
    fn cancel_login(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// Changes one of a provider's own settings.
    fn set_option(
        &self,
        provider: &str,
        account: &str,
        name: &str,
        value: &str,
    ) -> zbus::Result<()>;

    /// Validates a plugin file without storing it: what the import preview shows.
    fn inspect_plugin(&self, bytes: Vec<u8>) -> zbus::Result<PluginInfo>;

    /// Validates and installs a plugin file, replacing an earlier version of the same id.
    fn install_plugin(&self, bytes: Vec<u8>) -> zbus::Result<PluginInfo>;

    /// Removes an installed definition. Refused while any account still uses it.
    fn remove_plugin(&self, provider: &str) -> zbus::Result<()>;

    /// Sets one plugin account's complete metrics URL, and whether plain http is accepted
    /// for it. Per account, never shared: two accounts of one plugin are two endpoints.
    fn set_plugin_endpoint(
        &self,
        provider: &str,
        account: &str,
        endpoint: &str,
        allow_insecure_http: bool,
    ) -> zbus::Result<()>;

    /// Runs a plugin's parser against a local response fixture: the authoring loop, with no
    /// key read and no request made.
    fn render_plugin(&self, bytes: Vec<u8>, response: Vec<u8>) -> zbus::Result<Presentation>;

    /// Inspects secret-free local authentication candidates for one account.
    fn get_auth_sources(&self, provider: &str, account: &str) -> zbus::Result<Vec<AuthCandidate>>;

    /// Validates and stores one explicit authentication selection.
    fn select_auth_source(
        &self,
        provider: &str,
        account: &str,
        selection: AuthSelection,
    ) -> zbus::Result<()>;

    /// Switches notifications for one of an account's windows on or off.
    fn set_window_notify(
        &self,
        provider: &str,
        account: &str,
        window: &str,
        enabled: bool,
    ) -> zbus::Result<()>;

    fn set_release_check(&self, enabled: bool) -> zbus::Result<()>;
    fn set_minimize_on_close(&self, enabled: bool) -> zbus::Result<()>;
    fn set_theme(&self, theme: &str) -> zbus::Result<()>;
    fn set_startup_mode(&self, mode: &str) -> zbus::Result<()>;
    fn set_history_retention(&self, retention: &str) -> zbus::Result<()>;

    /// Chooses zone-based or fixed-interval polling for healthy accounts.
    fn set_refresh_mode(&self, mode: &str) -> zbus::Result<()>;

    /// Sets the fixed interval Manual mode polls at, in minutes.
    fn set_refresh_minutes(&self, minutes: u32) -> zbus::Result<()>;

    /// Chooses whether the client lays out as many card columns as the window fits.
    fn set_columns_auto(&self, enabled: bool) -> zbus::Result<()>;

    /// Sets the most card columns a window lays out while Auto is off, at least one.
    fn set_max_columns(&self, columns: u32) -> zbus::Result<()>;

    /// All three proxy settings at once: they are one setting, and half of it applied is a
    /// proxy nothing can be reached through.
    fn set_proxy(&self, mode: &str, host: &str, port: u16) -> zbus::Result<()>;

    fn clear_history(&self) -> zbus::Result<()>;

    /// Puts the configured providers in the order the user arranged the cards in.
    fn set_order(&self, providers: &[String]) -> zbus::Result<()>;

    /// Puts one provider's accounts in the order the user arranged them in.
    fn set_account_order(&self, provider: &str, accounts: Vec<String>) -> zbus::Result<()>;

    /// What the daemon on the other end is.
    #[zbus(property(emits_changed_signal = "false"))]
    fn version(&self) -> zbus::Result<String>;

    /// The installed plugin definitions changed.
    #[zbus(signal)]
    fn plugins_changed(&self, plugins: Vec<PluginInfo>) -> zbus::Result<()>;

    /// One account changed.
    #[zbus(signal)]
    fn provider_changed(&self, status: ProviderStatus) -> zbus::Result<()>;

    /// One configured account was removed.
    #[zbus(signal)]
    fn provider_removed(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// The configured providers are now in this order.
    #[zbus(signal)]
    fn order_changed(&self, providers: Vec<String>) -> zbus::Result<()>;

    /// Application preferences changed.
    #[zbus(signal)]
    fn preferences_changed(&self, preferences: Preferences) -> zbus::Result<()>;

    /// Paths or storage facts changed.
    #[zbus(signal)]
    fn data_changed(&self, data: DataInfo) -> zbus::Result<()>;

    /// Availability of a newer published application release changed.
    #[zbus(signal)]
    fn update_changed(&self, version: &str) -> zbus::Result<()>;

    /// Another client asked this one to come forward.
    #[zbus(signal)]
    fn activate_requested(&self) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use tidemark_types::{AccountId, ProviderId, ProviderStatus, ids};

    /// A daemon that answers two of the real methods.
    struct FakeDaemon;

    #[zbus::interface(name = "io.github.zbndev.Tidemark.Daemon1")]
    impl FakeDaemon {
        async fn get_status(&self) -> Vec<ProviderStatus> {
            vec![ProviderStatus::pending(
                &ProviderId::new("zai".to_owned()),
                &AccountId::default(),
            )]
        }

        #[zbus(property(emits_changed_signal = "false"))]
        async fn version(&self) -> String {
            "0.0.0-test".to_owned()
        }
    }

    #[test]
    fn the_proxy_reads_a_daemon_serving_the_interface() {
        async_io::block_on(async {
            let Ok(connection) = zbus::Connection::session().await else {
                eprintln!("skipped: no session bus reachable");
                return;
            };
            let name = format!("io.github.zbndev.TidemarkIpcTest{}", std::process::id());
            let server = zbus::connection::Builder::session()
                .expect("session builder")
                .name(name.as_str())
                .expect("unique test name")
                .serve_at(ids::OBJECT_PATH, FakeDaemon)
                .expect("serves the object")
                .build()
                .await
                .expect("test service starts");

            let proxy = super::DaemonProxy::builder(&connection)
                .destination(name.as_str())
                .expect("valid destination")
                .build()
                .await
                .expect("proxy builds");

            assert_eq!(proxy.version().await.expect("version"), "0.0.0-test");
            let statuses = proxy.get_status().await.expect("status");
            assert_eq!(statuses.len(), 1);
            assert_eq!(statuses[0].provider, "zai");
            assert_eq!(statuses[0].state, "pending");

            drop(server);
        });
    }
}
