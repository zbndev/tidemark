//! The `tidemarkctl` binary: parse, run one command, become its exit code.
//!
//! Everything it calls lives in the library beside it, where the integration tests can
//! reach it. See `lib.rs` for why.

use std::process::ExitCode;

use clap::Parser;

use tidemark_cli::exit::{Exit, Failure};
use tidemark_cli::titles::Titles;
use tidemark_cli::{cli, commands, connect, format, guard, watch};

fn main() -> ExitCode {
    let parsed = cli::Cli::parse();
    match async_io::block_on(run(parsed)) {
        Ok(exit) => ExitCode::from(exit.code()),
        Err(failure) => {
            eprintln!("{}", failure.message);
            ExitCode::from(failure.exit.code())
        }
    }
}

async fn run(cli: cli::Cli) -> Result<Exit, Failure> {
    match cli.command {
        cli::Command::Version => {
            let proxy = connect::daemon().await?;
            println!("tidemarkctl {}", env!("CARGO_PKG_VERSION"));
            println!("tidemarkd {}", proxy.version().await?);
            Ok(Exit::Ok)
        }
        cli::Command::Usage(args) => {
            let proxy = connect::daemon().await?;
            let statuses = proxy.get_status().await?;
            let selected =
                format::select(&statuses, args.provider.as_deref(), args.account.as_deref());
            match args.format {
                cli::Format::Text => print!(
                    "{}",
                    format::text::render(
                        &selected,
                        &Titles::fetch(&proxy).await?,
                        tidemark_types::Timestamp::now()
                    )
                ),
                cli::Format::Json => println!("{}", format::json::render(&selected)?),
                cli::Format::Waybar => println!(
                    "{}",
                    format::waybar::render(
                        &selected,
                        &Titles::fetch(&proxy).await?,
                        tidemark_types::Timestamp::now()
                    )?
                ),
            }
            Ok(Exit::Ok)
        }
        cli::Command::Guard(args) => {
            let proxy = connect::daemon().await?;
            let statuses = proxy.get_status().await?;
            let selected =
                format::select(&statuses, args.provider.as_deref(), args.account.as_deref());
            let selection = match (args.any, args.window) {
                (true, _) => guard::Selection::Any,
                (false, Some(key)) => guard::Selection::Named(key),
                (false, None) => guard::Selection::Dominant,
            };
            let verdict = guard::decide(&selected, &selection, args.min_remaining);
            match &verdict {
                guard::Verdict::Safe { remaining } | guard::Verdict::Below { remaining } => {
                    println!("{} left", tidemark_types::present::percent(*remaining));
                }
                guard::Verdict::Unavailable(reason) => eprintln!("{reason}"),
            }
            Ok(verdict.exit())
        }
        cli::Command::Watch(args) => {
            let sink = match args.format {
                cli::StreamFormat::Json => watch::Sink::Events,
                cli::StreamFormat::Waybar => watch::Sink::Waybar,
            };
            watch::run(sink, args.provider).await
        }
        cli::Command::Provider { command } => {
            let proxy = connect::daemon().await?;
            commands::provider::run(&proxy, command).await
        }
        cli::Command::Account { command } => {
            let proxy = connect::daemon().await?;
            commands::account::run(&proxy, command).await
        }
        cli::Command::Auth { command } => {
            let proxy = connect::daemon().await?;
            commands::auth::run(&proxy, command).await
        }
        cli::Command::Refresh { provider } => {
            let proxy = connect::daemon().await?;
            proxy.refresh(provider.as_deref().unwrap_or("")).await?;
            Ok(Exit::Ok)
        }
        cli::Command::Option {
            provider,
            account,
            name,
            value,
        } => {
            let proxy = connect::daemon().await?;
            proxy.set_option(&provider, &account, &name, &value).await?;
            Ok(Exit::Ok)
        }
        cli::Command::Notify {
            provider,
            account,
            window,
            enabled,
        } => {
            let proxy = connect::daemon().await?;
            proxy
                .set_window_notify(&provider, &account, &window, enabled.into())
                .await?;
            Ok(Exit::Ok)
        }
        cli::Command::Config { command } => {
            let proxy = connect::daemon().await?;
            commands::config::run(&proxy, command).await
        }
        cli::Command::History { command } => {
            let proxy = connect::daemon().await?;
            match command {
                cli::HistoryCommand::Segment {
                    provider,
                    account,
                    window,
                } => {
                    for point in proxy.current_segment(&provider, &account, &window).await? {
                        println!(
                            "{} {}",
                            point.captured_at,
                            tidemark_types::present::percent(point.used_percent)
                        );
                    }
                }
                cli::HistoryCommand::Clear => proxy.clear_history().await?,
            }
            Ok(Exit::Ok)
        }
        cli::Command::Data => {
            let proxy = connect::daemon().await?;
            let data = proxy.get_data_info().await?;
            println!("config              {}", data.config_path);
            println!("history             {}", data.history_path);
            println!("history-bytes       {}", data.history_bytes);
            println!("key-schema          {}", data.key_schema);
            println!("token-schema        {}", data.token_schema);
            // Not the `release-check` of `config show`: that is the user's switch, this is
            // whether the build has the checker at all.
            println!("release-check-built {}", data.release_check_available);
            Ok(Exit::Ok)
        }
        cli::Command::Update => {
            let proxy = connect::daemon().await?;
            let version = proxy.get_update().await?;
            if version.is_empty() {
                println!("no newer release is known");
            } else {
                println!("{version}");
            }
            Ok(Exit::Ok)
        }
    }
}
