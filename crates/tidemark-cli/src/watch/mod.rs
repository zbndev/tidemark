//! The daemon's signals as a line-oriented stream.
//!
//! One JSON object per line, flushed as it is written, so a plugin can read a pipe instead
//! of polling a snapshot every few seconds. The first line of a connection is always a
//! snapshot, and so is the first line after a reconnect: whatever the daemon published
//! while nothing was listening was announced to nobody, and re-reading is the only honest
//! recovery.

pub mod mirror;

use serde::Serialize;
use tidemark_types::{DataInfo, Preferences, ProviderStatus};

/// One line of the stream.
#[expect(
    dead_code,
    reason = "the watch command constructs every variant, in the next commit"
)]
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event<'a> {
    Snapshot { accounts: &'a [ProviderStatus] },
    Changed { account: &'a ProviderStatus },
    Removed { provider: &'a str, account: &'a str },
    Order { providers: &'a [String] },
    Preferences { preferences: &'a Preferences },
    Data { data: &'a DataInfo },
    Update { version: &'a str },
    Waiting,
}

/// One event, one line. The published wire types carry their D-Bus signatures under
/// `serde_json`, so the line goes through [`crate::format::payload`] — the same peeling
/// `usage --format json` does, because a plugin reading the stream and a plugin reading a
/// snapshot must parse the same shapes.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the watch command is the caller, in the next commit"
    )
)]
pub fn line(event: &Event<'_>) -> Result<String, serde_json::Error> {
    serde_json::to_string(&crate::format::payload(serde_json::to_value(event)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId};

    #[test]
    fn a_snapshot_names_itself_and_carries_every_account() {
        let accounts = vec![ProviderStatus::pending(
            &ProviderId::new("zai".to_owned()),
            &AccountId::default(),
        )];
        let text = line(&Event::Snapshot {
            accounts: &accounts,
        })
        .expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["event"], "snapshot");
        assert_eq!(value["accounts"][0]["provider"], "zai");
        assert!(!text.contains('\n'), "one event is one line: {text}");
    }

    #[test]
    fn a_removal_names_the_account_that_went_away() {
        let text = line(&Event::Removed {
            provider: "claude",
            account: "work",
        })
        .expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(value["event"], "removed");
        assert_eq!(value["provider"], "claude");
        assert_eq!(value["account"], "work");
    }

    #[test]
    fn waiting_is_a_line_of_its_own() {
        let text = line(&Event::Waiting).expect("serializes");
        assert_eq!(text, r#"{"event":"waiting"}"#);
    }
}
