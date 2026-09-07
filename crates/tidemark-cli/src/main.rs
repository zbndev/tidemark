//! The `tidemarkctl` binary: parse, run one command, become its exit code.
//!
//! Everything it calls lives in the library beside it, where the integration tests can
//! reach it. See `lib.rs` for why.

use std::process::ExitCode;

use clap::Parser;

use tidemark_cli::exit::{Exit, Failure};
use tidemark_cli::{cli, connect, format, guard, watch};

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
                    format::text::render(&selected, tidemark_types::Timestamp::now())
                ),
                cli::Format::Json => println!("{}", format::json::render(&selected)?),
                cli::Format::Waybar => println!(
                    "{}",
                    format::waybar::render(&selected, tidemark_types::Timestamp::now())?
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
    }
}
