//! Credentials, from the outside.
//!
//! Nothing here reads a keyring, a cookie database or a token file: the daemon owns every
//! credential, and this only hands it a value or asks it what it can see. What it prints
//! back is exactly what the daemon published — titles and readiness, never a secret.

use std::io::Write;

use tidemark_ipc::DaemonProxy;
use tidemark_types::{AuthCandidate, AuthSelection};

use crate::cli::AuthCommand;
use crate::exit::{Exit, Failure};
use crate::secret;

pub async fn run(proxy: &DaemonProxy<'_>, command: AuthCommand) -> Result<Exit, Failure> {
    match command {
        AuthCommand::SetKey {
            provider,
            account,
            key_file,
        } => {
            let key = secret::read(source(key_file))?;
            proxy.set_key(&provider, &account, &key).await?;
        }
        AuthCommand::SetSession {
            provider,
            account,
            key_file,
        } => {
            let session = secret::read(source(key_file))?;
            proxy.set_session(&provider, &account, &session).await?;
        }
        AuthCommand::SignOut { provider, account } => proxy.sign_out(&provider, &account).await?,
        // The URL first and flushed, because the caller — a person or a plugin — cannot act
        // on it until it is on the pipe, and `AwaitLogin` then blocks for as long as the
        // browser takes. The daemon never opens a browser and neither does this: it may be
        // running on a machine with no display.
        AuthCommand::Login { provider, account } => {
            let url = proxy.begin_login(&provider, &account).await?;
            println!("{url}");
            std::io::stdout().flush()?;
            proxy.await_login(&provider, &account).await?;
        }
        AuthCommand::CancelLogin { provider, account } => {
            proxy.cancel_login(&provider, &account).await?
        }
        AuthCommand::Sources { provider, account } => {
            print!(
                "{}",
                sources(&proxy.get_auth_sources(&provider, &account).await?)
            );
        }
        AuthCommand::Select {
            provider,
            account,
            mode,
            candidate,
        } => {
            proxy
                .select_auth_source(&provider, &account, AuthSelection { mode, candidate })
                .await?
        }
    }
    Ok(Exit::Ok)
}

fn source(key_file: Option<std::path::PathBuf>) -> secret::Source {
    match key_file {
        Some(path) => secret::Source::File(path),
        None => secret::Source::Stdin,
    }
}

/// The id first, because that is the word `auth select --candidate` takes, then the name a
/// person recognises the source by. Never a cookie value, a token or a database path — the
/// daemon does not publish those, and this prints exactly what it publishes.
///
/// A string rather than a `println!` per row so the shape can be tested without a daemon.
fn sources(candidates: &[AuthCandidate]) -> String {
    let mut out = String::new();
    rows(&mut out, candidates, 0);
    out
}

/// A profile inside a browser is a child, and its indent is what says so.
fn rows(out: &mut String, candidates: &[AuthCandidate], depth: usize) {
    for candidate in candidates {
        out.push_str(&format!(
            "{}{:<28} {:<24} {:<12} {}\n",
            "  ".repeat(depth),
            candidate.id,
            candidate.title,
            candidate.state,
            candidate.subtitle.as_deref().unwrap_or("")
        ));
        rows(out, &candidate.children, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, title: &str, children: Vec<AuthCandidate>) -> AuthCandidate {
        AuthCandidate {
            id: id.to_owned(),
            title: title.to_owned(),
            subtitle: None,
            state: "ready".to_owned(),
            children,
        }
    }

    /// The nesting is the whole information in a browser with two usable profiles: which
    /// id belongs to which browser, so `auth select --candidate` can name one.
    #[test]
    fn a_profile_inside_a_browser_is_indented_under_it() {
        let out = sources(&[candidate(
            "firefox",
            "Firefox",
            vec![candidate("firefox:default", "default", Vec::new())],
        )]);
        let mut lines = out.lines();
        assert!(
            lines.next().expect("a browser row").starts_with("firefox "),
            "{out}"
        );
        assert!(
            lines
                .next()
                .expect("a profile row")
                .starts_with("  firefox:default "),
            "{out}"
        );
    }

    /// An absent subtitle prints as nothing, not as a word standing in for one.
    #[test]
    fn a_source_without_context_gets_no_invented_context() {
        let out = sources(&[candidate("chrome", "Chrome", Vec::new())]);
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(out.trim_end().ends_with("ready"), "{out}");
    }
}
