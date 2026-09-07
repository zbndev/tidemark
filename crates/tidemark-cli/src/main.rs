//! `tidemarkctl`: the third consumer of the daemon's interface, after the window and
//! `busctl`.
//!
//! It performs no provider I/O — every number it prints came off the bus — and it holds no
//! runtime: zbus's async-io backend drives its own connection thread, so one `block_on` at
//! the top is the whole of this program's concurrency.

mod cli;
mod connect;
mod exit;

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
    }
}
