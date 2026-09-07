//! The mutating commands, driven against a daemon that records what it was asked.
//!
//! What is asserted is the *call*, not a printed line: these commands exist to reach the
//! daemon with the arguments the user typed, and the daemon owns everything after that.

mod fake;

use tidemark_cli::cli::{AccountCommand, ConfigCommand, ProviderCommand};
use tidemark_cli::commands;

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
