//! The mutating commands, driven against a daemon that records what it was asked.
//!
//! What is asserted is the *call*, not a printed line: these commands exist to reach the
//! daemon with the arguments the user typed, and the daemon owns everything after that.

mod fake;

use tidemark_cli::cli::{AccountCommand, ProviderCommand};
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
