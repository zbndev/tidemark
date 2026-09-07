//! `tidemarkctl`: the third consumer of the daemon's interface, after the window and
//! `busctl`.
//!
//! It performs no provider I/O — every number it prints came off the bus — and it holds no
//! runtime: zbus's async-io backend drives its own connection thread, so one `block_on` at
//! the top is the whole of this program's concurrency.

mod cli;
mod connect;
mod exit;
mod format;
mod guard;
mod watch;

use std::process::ExitCode;

use clap::Parser;

use crate::exit::{Exit, Failure};

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
    }
}
