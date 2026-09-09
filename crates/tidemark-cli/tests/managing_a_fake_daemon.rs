//! The mutating commands, driven against a daemon that records what it was asked.
//!
//! What is asserted is the *call*, not a printed line: these commands exist to reach the
//! daemon with the arguments the user typed, and the daemon owns everything after that.

mod fake;

use tidemark_cli::cli::{
    AccountCommand, ConfigCommand, PluginCommand, PluginFormat, ProviderCommand,
};
use tidemark_cli::commands;
use tidemark_cli::exit::Exit;

#[test]
fn adding_and_ordering_reach_the_daemon_with_the_arguments_given() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        commands::provider::run(
            &proxy,
            ProviderCommand::Add {
                provider: "codex".to_owned(),
            },
        )
        .await
        .expect("add succeeds");

        commands::provider::run(
            &proxy,
            ProviderCommand::Order {
                providers: vec!["codex".to_owned(), "claude".to_owned()],
            },
        )
        .await
        .expect("order succeeds");

        commands::account::run(
            &proxy,
            AccountCommand::Rename {
                provider: "claude".to_owned(),
                account: "default".to_owned(),
                new: "work".to_owned(),
            },
        )
        .await
        .expect("rename succeeds");

        commands::account::run(
            &proxy,
            AccountCommand::Order {
                provider: "claude".to_owned(),
                accounts: vec!["work".to_owned(), "personal".to_owned()],
            },
        )
        .await
        .expect("account order succeeds");

        assert_eq!(
            calls.recorded(),
            vec![
                "AddProvider(codex)".to_owned(),
                "SetOrder(codex,claude)".to_owned(),
                "RenameAccount(claude,default,work)".to_owned(),
                "SetAccountOrder(claude,work,personal)".to_owned(),
            ]
        );
        drop(server);
    });
}

/// `provider rm` and `account rm` are the same daemon call, and the daemon removes the
/// credential and the card with the account. A test that let them drift apart would be a
/// test of nothing.
#[test]
fn removing_an_account_is_one_call_whichever_noun_the_user_reached_for() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        commands::provider::run(
            &proxy,
            ProviderCommand::Rm {
                provider: "claude".to_owned(),
                account: "default".to_owned(),
            },
        )
        .await
        .expect("provider rm succeeds");

        commands::account::run(
            &proxy,
            AccountCommand::Rm {
                provider: "claude".to_owned(),
                account: "default".to_owned(),
            },
        )
        .await
        .expect("account rm succeeds");

        let recorded = calls.recorded();
        assert_eq!(
            recorded,
            vec![
                "RemoveProvider(claude,default)".to_owned(),
                "RemoveProvider(claude,default)".to_owned(),
            ]
        );
        drop(server);
    });
}

#[test]
fn an_order_with_no_providers_is_refused_before_the_daemon_hears_it() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        let failure = commands::provider::run(
            &proxy,
            ProviderCommand::Order {
                providers: Vec::new(),
            },
        )
        .await
        .expect_err("an empty order is not a permutation");

        assert_eq!(failure.exit, tidemark_cli::exit::Exit::Usage);
        assert!(calls.recorded().is_empty(), "{:?}", calls.recorded());
        drop(server);
    });
}

#[test]
fn an_account_order_with_no_accounts_is_refused_too() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        let failure = commands::account::run(
            &proxy,
            AccountCommand::Order {
                provider: "claude".to_owned(),
                accounts: Vec::new(),
            },
        )
        .await
        .expect_err("an empty order is not a permutation");

        assert_eq!(failure.exit, tidemark_cli::exit::Exit::Usage);
        assert!(calls.recorded().is_empty(), "{:?}", calls.recorded());
        drop(server);
    });
}

#[test]
fn a_proxy_is_sent_as_one_setting_and_a_half_of_one_is_refused() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        let failure = commands::config::run(
            &proxy,
            ConfigCommand::Proxy {
                mode: "socks5".to_owned(),
                host: Some("127.0.0.1".to_owned()),
                port: None,
            },
        )
        .await
        .expect_err("half a proxy is not a proxy");
        assert_eq!(failure.exit, tidemark_cli::exit::Exit::Usage);
        assert!(calls.recorded().is_empty(), "{:?}", calls.recorded());

        commands::config::run(
            &proxy,
            ConfigCommand::Proxy {
                mode: "socks5".to_owned(),
                host: Some("127.0.0.1".to_owned()),
                port: Some(1080),
            },
        )
        .await
        .expect("a whole proxy is accepted");
        assert_eq!(
            calls.recorded(),
            vec!["SetProxy(socks5,127.0.0.1,1080)".to_owned()]
        );

        commands::config::run(
            &proxy,
            ConfigCommand::Proxy {
                mode: "off".to_owned(),
                host: None,
                port: None,
            },
        )
        .await
        .expect("off needs neither");
        assert_eq!(
            calls.recorded().last().map(String::as_str),
            Some("SetProxy(off,,0)")
        );

        drop(server);
    });
}

/// The interval must be sent *before* the mode, because `SetRefreshMode` polls every
/// account immediately: the other order would poll once at the interval the user was
/// leaving behind.
#[test]
fn a_manual_interval_is_sent_before_the_mode_that_starts_using_it() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        commands::config::run(
            &proxy,
            ConfigCommand::Refresh {
                mode: "manual".to_owned(),
                minutes: Some(15),
            },
        )
        .await
        .expect("sets both");

        assert_eq!(
            calls.recorded(),
            vec![
                "SetRefreshMinutes(15)".to_owned(),
                "SetRefreshMode(manual)".to_owned(),
            ]
        );
        drop(server);
    });
}

/// A plugin file the CLI only ever reads and forwards: it never parses one, so its content
/// matters here only in that the whole of it must arrive at the daemon.
const MINIMAL_PLUGIN: &str = r#"
[provider]
id = "com.acme.quota"
name = "Acme AI"
version = "1.2.0"
"#;

/// Writes a file into a directory that removes itself, and hands back both.
fn write_temp(name: &str, contents: &str) -> (std::path::PathBuf, TempDir) {
    let dir = TempDir::new();
    let path = dir.0.join(name);
    std::fs::write(&path, contents).expect("the fixture is written");
    (path, dir)
}

#[derive(Debug)]
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "tidemarkctl-plugin-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("a temporary directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn validating_a_plugin_sends_the_files_bytes_and_never_installs() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };
        let (file, _dir) = write_temp("acme.tidemark-provider", MINIMAL_PLUGIN);

        commands::plugin::run(
            &proxy,
            PluginCommand::Validate {
                file,
                format: PluginFormat::Text,
            },
        )
        .await
        .expect("validation succeeds");

        assert_eq!(
            calls.recorded(),
            vec![format!("InspectPlugin({} bytes)", MINIMAL_PLUGIN.len())],
            "validation inspects and does nothing else"
        );
        drop(server);
    });
}

#[test]
fn a_rejected_plugin_fails_with_the_daemons_own_words() {
    async_io::block_on(async {
        let Some((server, proxy, _calls)) =
            fake::serve(fake::FakeDaemon::refusing("parser.language: must be lua54")).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };
        let (file, _dir) = write_temp("bad.tidemark-provider", MINIMAL_PLUGIN);

        let failure = commands::plugin::run(
            &proxy,
            PluginCommand::Validate {
                file,
                format: PluginFormat::Text,
            },
        )
        .await
        .expect_err("a refused plugin is a failure");

        assert_eq!(failure.exit, Exit::Daemon);
        assert!(failure.message.contains("parser.language"), "{failure:?}");
        drop(server);
    });
}

#[test]
fn rendering_sends_both_local_files_and_never_reads_a_key() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };
        let (file, _plugin_dir) = write_temp("acme.tidemark-provider", MINIMAL_PLUGIN);
        let response = r#"{"used": 1, "limit": 4}"#;
        let (fixture, _response_dir) = write_temp("acme.json", response);

        commands::plugin::run(
            &proxy,
            PluginCommand::Render {
                file,
                response: fixture,
                format: PluginFormat::Json,
            },
        )
        .await
        .expect("rendering succeeds");

        assert_eq!(
            calls.recorded(),
            vec![format!(
                "RenderPlugin({} bytes,{} bytes)",
                MINIMAL_PLUGIN.len(),
                response.len()
            )]
        );
        drop(server);
    });
}

#[test]
fn a_file_the_cli_cannot_read_is_not_the_daemons_problem() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        let failure = commands::plugin::run(
            &proxy,
            PluginCommand::Validate {
                file: "/no/such/file.tidemark-provider".into(),
                format: PluginFormat::Text,
            },
        )
        .await
        .expect_err("a missing file is a failure");

        assert_eq!(failure.exit, Exit::Usage);
        assert!(failure.message.contains("/no/such/file"), "{failure:?}");
        assert!(
            calls.recorded().is_empty(),
            "the daemon was never asked: {:?}",
            calls.recorded()
        );
        drop(server);
    });
}

#[test]
fn an_endpoint_defaults_to_the_default_account_and_to_refusing_plain_http() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        commands::plugin::run(
            &proxy,
            PluginCommand::Endpoint {
                provider: "com.acme.quota".to_owned(),
                url: "https://a.test/u".to_owned(),
                account: "default".to_owned(),
                allow_insecure_http: false,
            },
        )
        .await
        .expect("the endpoint is set");

        commands::plugin::run(
            &proxy,
            PluginCommand::Endpoint {
                provider: "com.acme.quota".to_owned(),
                url: "http://a.test/u".to_owned(),
                account: "work".to_owned(),
                allow_insecure_http: true,
            },
        )
        .await
        .expect("the acknowledged endpoint is set");

        assert_eq!(
            calls.recorded(),
            vec![
                "SetPluginEndpoint(com.acme.quota,default,https://a.test/u,false)".to_owned(),
                "SetPluginEndpoint(com.acme.quota,work,http://a.test/u,true)".to_owned(),
            ]
        );
        drop(server);
    });
}

#[test]
fn listing_reads_the_one_catalog_and_removing_names_the_provider() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_plugin()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        commands::plugin::run(
            &proxy,
            PluginCommand::List {
                format: PluginFormat::Text,
            },
        )
        .await
        .expect("listing succeeds");

        commands::plugin::run(
            &proxy,
            PluginCommand::Remove {
                provider: "com.acme.quota".to_owned(),
            },
        )
        .await
        .expect("removal succeeds");

        assert_eq!(
            calls.recorded(),
            vec![
                "ListProviders".to_owned(),
                "RemovePlugin(com.acme.quota)".to_owned(),
            ],
            "there is no plugin-only listing method: the catalog is the listing"
        );
        drop(server);
    });
}
