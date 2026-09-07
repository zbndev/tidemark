//! What a credential command sends, and what it must never say out loud.

mod fake;

use tidemark_cli::cli::AuthCommand;
use tidemark_cli::commands;

#[test]
fn a_key_from_a_file_reaches_the_daemon_and_is_never_printed() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        let path = std::env::temp_dir().join(format!("tidemarkctl-key-{}", std::process::id()));
        std::fs::write(&path, "sk-secret-value\n").expect("writes the key file");

        commands::auth::run(
            &proxy,
            AuthCommand::SetKey {
                provider: "claude".to_owned(),
                account: "default".to_owned(),
                key_file: Some(path.clone()),
            },
        )
        .await
        .expect("stores the key");

        let recorded = calls.recorded();
        assert_eq!(recorded, vec!["SetKey(claude,default,15 chars)".to_owned()]);
        assert!(
            !recorded.iter().any(|call| call.contains("sk-secret")),
            "a secret must never reach a log or a test name: {recorded:?}"
        );

        std::fs::remove_file(path).ok();
        drop(server);
    });
}

/// An unreadable key file stops before the bus: a `SetKey` that stored the empty string,
/// or a D-Bus error about a value the user never managed to supply, are both worse than
/// the sentence naming the file.
#[test]
fn a_key_file_that_is_not_there_never_becomes_a_daemon_call() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        let path = std::env::temp_dir().join("tidemarkctl-key-does-not-exist");
        std::fs::remove_file(&path).ok();

        let failure = commands::auth::run(
            &proxy,
            AuthCommand::SetKey {
                provider: "claude".to_owned(),
                account: "default".to_owned(),
                key_file: Some(path),
            },
        )
        .await
        .expect_err("there is no key to store");

        assert_eq!(failure.exit, tidemark_cli::exit::Exit::Usage);
        assert!(calls.recorded().is_empty(), "{:?}", calls.recorded());
        drop(server);
    });
}

/// `sign_out` is the one credential command with nothing to read, and the account it names
/// must be the account the daemon is told to clear.
#[test]
fn signing_out_names_the_account_it_was_given() {
    async_io::block_on(async {
        let Some((server, proxy, calls)) = fake::serve(fake::FakeDaemon::with_one_account()).await
        else {
            eprintln!("skipped: no session bus reachable");
            return;
        };

        commands::auth::run(
            &proxy,
            AuthCommand::SignOut {
                provider: "zai".to_owned(),
                account: "tanya".to_owned(),
            },
        )
        .await
        .expect("signs out");

        assert_eq!(calls.recorded(), vec!["SignOut(zai,tanya)".to_owned()]);
        drop(server);
    });
}
