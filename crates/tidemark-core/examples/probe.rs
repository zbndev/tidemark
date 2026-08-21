//! Debug entry point: fetch one provider once and print what came back.
//!
//! The key is read from **stdin**, never from an argument, because arguments are visible
//! in `ps` to every process on the machine:
//!
//! ```sh
//! pass my/zai | cargo run -p tidemark-core --example probe
//! pass my/zai | cargo run -p tidemark-core --example probe -- --region cn
//! ```
//!
//! This is a development tool, not a product surface. The real entry points are `tidemarkd`
//! and the GUI.

use std::io::Read;
use tidemark_core::providers::keyed::Options;
use tidemark_core::providers::{Credential, Provider, keyed, zai};
use tidemark_types::{Snapshot, Timestamp, Window};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let region = match std::env::args().nth(1).as_deref() {
        None => zai::Region::Global,
        Some("--region") => match std::env::args().nth(2).as_deref() {
            Some("global") | None => zai::Region::Global,
            Some("cn") => zai::Region::BigModelCn,
            Some(other) => {
                eprintln!("unknown region {other}; expected global or cn");
                return std::process::ExitCode::FAILURE;
            }
        },
        Some(other) => {
            eprintln!("unexpected argument {other}; usage: probe [--region global|cn]");
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut key = String::new();
    if std::io::stdin().read_to_string(&mut key).is_err() {
        eprintln!("could not read the key from stdin");
        return std::process::ExitCode::FAILURE;
    }

    let options: Options = [(zai::REGION.to_owned(), region.as_value().to_owned())]
        .into_iter()
        .collect();
    let provider = match keyed::Keyed::new(&zai::SPEC, Credential::new(key.trim()), &options) {
        Ok(provider) => provider,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    eprintln!("GET {}", provider.url());
    match provider.fetch().await {
        Ok(snapshot) => {
            print(&snapshot);
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed: {e}");
            if let Some(wait) = e.retry_after() {
                eprintln!("the provider asked for {} seconds", wait.as_secs());
            }
            std::process::ExitCode::FAILURE
        }
    }
}

fn print(snapshot: &Snapshot) {
    let now = snapshot.captured_at;
    let dominant = snapshot.dominant_window().map(|w| w.key.clone());
    println!("{}/{}", snapshot.provider, snapshot.account);

    for window in &snapshot.windows {
        let lead = if Some(&window.key) == dominant.as_ref() {
            '>'
        } else {
            ' '
        };
        println!(
            "{lead} {:<14} {:<10} {:>6.1}% used   {:<12} {:<10} {}",
            window.key.as_str(),
            window.title,
            window.used_percent,
            pace(window, now),
            length(window),
            resets(window, now),
        );
    }

    for section in &snapshot.details {
        println!("\n  {}", section.title);
        for row in &section.rows {
            println!("    {:<28} {}", row.label, row.value);
        }
    }
}

fn pace(window: &Window, now: Timestamp) -> String {
    match (window.pace(now), window.is_outpacing(now)) {
        (Some(pace), Some(true)) => format!("pace {:.0}% AHEAD", pace * 100.0),
        (Some(pace), _) => format!("pace {:.0}%", pace * 100.0),
        // Two of the five providers do not describe every window's length. A missing pace
        // mark is a state to render, not a number to invent.
        (None, _) => "pace unknown".to_owned(),
    }
}

fn length(window: &Window) -> String {
    window
        .length
        .map(|l| duration(l.as_secs() as i64))
        .unwrap_or_else(|| "?".to_owned())
}

fn resets(window: &Window, now: Timestamp) -> String {
    match window.seconds_until_reset(now) {
        Some(0) => "resets now".to_owned(),
        Some(seconds) => format!("resets in {}", duration(seconds)),
        None => "no reset time".to_owned(),
    }
}

fn duration(seconds: i64) -> String {
    let minutes = seconds / 60;
    match (minutes / 1440, (minutes / 60) % 24, minutes % 60) {
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}
