//! Usage as another program parses it.
//!
//! An object with one key rather than a bare array, so a later build can add a second key
//! without breaking a plugin that already reads this one — the same reason every published
//! D-Bus structure is a dictionary. The account objects are `ProviderStatus` under its own
//! field names, which means **absent stays absent**: a provider that withheld a reset time
//! produces no key at all, and no `null` invites a plugin to render it as zero.

use serde::Serialize;
use tidemark_types::ProviderStatus;

/// The published document.
#[derive(Debug, Serialize)]
struct Document<'a> {
    accounts: &'a [&'a ProviderStatus],
}

pub fn render(statuses: &[&ProviderStatus]) -> Result<String, serde_json::Error> {
    let document = serde_json::to_value(Document { accounts: statuses })?;
    serde_json::to_string_pretty(&super::payload(document))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tidemark_types::{
        AccountId, DetailRow, DetailSection, ProviderId, ProviderState, WindowStatus,
    };

    #[test]
    fn an_account_with_no_reading_publishes_no_reading_keys() {
        let status =
            ProviderStatus::pending(&ProviderId::new("zai".to_owned()), &AccountId::default());
        let text = render(&[&status]).expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let account = &value["accounts"][0];
        assert_eq!(account["provider"], "zai");
        assert_eq!(account["state"], "pending");
        assert!(account.get("captured_at").is_none(), "{text}");
        assert!(account.get("message").is_none(), "{text}");
    }

    #[test]
    fn a_window_publishes_the_keys_a_plugin_parses() {
        let mut status =
            ProviderStatus::pending(&ProviderId::new("claude".to_owned()), &AccountId::default());
        status.state = ProviderState::Ok.as_wire().to_owned();
        status.captured_at = Some(1_785_704_400);
        status.windows = vec![WindowStatus {
            key: "w18000".to_owned(),
            title: "Session".to_owned(),
            subtitle: None,
            used_percent: 72.0,
            resets_at: Some(1_785_708_000),
            length_secs: Some(18_000),
        }];
        let text = render(&[&status]).expect("serializes");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let window = &value["accounts"][0]["windows"][0];
        assert_eq!(window["key"], "w18000");
        assert_eq!(window["used_percent"], 72.0);
        assert_eq!(window["resets_at"], 1_785_708_000_i64);
        assert_eq!(window["length_secs"], 18_000);
        assert!(window.get("subtitle").is_none(), "{text}");
    }

    /// `details` is the deepest thing published: a list of sections, each with a list of
    /// rows, and `SerializeDict` wraps the list *and* is the reason its contents need
    /// unwrapping. A plugin reads `.details[0].rows[0].value`, not a signature.
    #[test]
    fn details_arrive_as_plain_objects_all_the_way_down() {
        let mut status =
            ProviderStatus::pending(&ProviderId::new("zai".to_owned()), &AccountId::default());
        status.details = vec![DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Level".to_owned(),
                value: "pro".to_owned(),
            }],
        }];
        let text = render(&[&status]).expect("serializes");
        let value: Value = serde_json::from_str(&text).expect("valid json");
        let section = &value["accounts"][0]["details"][0];
        assert_eq!(section["title"], "Plan", "{text}");
        assert_eq!(section["rows"][0]["label"], "Level", "{text}");
        assert_eq!(section["rows"][0]["value"], "pro", "{text}");
    }
}
