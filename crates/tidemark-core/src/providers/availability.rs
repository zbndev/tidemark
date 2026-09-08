//! Provider-declared quota ladders.
//!
//! A longer window only disables a shorter one when both belong to the same quota pool.
//! Window duration alone is insufficient: many providers publish separate model, MCP, or
//! product allowances with the same cadence.

use tidemark_types::{Snapshot, Window, WindowKey};

/// The providers whose APIs expose one consumable rate-limit ladder as five-hour, weekly,
/// and, where present, monthly windows. Providers outside this list may expose windows of the
/// same durations for unrelated products, so their readings must remain independent.
fn has_limit_ladder(provider: &str) -> bool {
    matches!(
        provider,
        "alibaba"
            | "antigravity"
            | "claude"
            | "clinepass"
            | "codex"
            | "commandcode"
            | "factory"
            | "kimi"
            | "minimax"
            | "ollama"
            | "opencode"
            | "opencodego"
            | "sakana"
            | "stepfun"
            | "sub2api"
            | "zai"
            | "zenmux"
    )
}

/// Returns the full, longer window that makes `candidate` unusable.
///
/// The closest full parent wins so a five-hour bar explains its weekly ceiling even when a
/// monthly ceiling is also full. Kimi is the one documented exception to the pool-key rule:
/// its burst limiter is `rate/w18000`, while the shared plan allowance is `w604800`.
pub fn blocked_by<'a>(snapshot: &'a Snapshot, candidate: &Window) -> Option<&'a Window> {
    if !has_limit_ladder(snapshot.provider.as_str()) {
        return None;
    }
    let candidate_length = candidate.length?.as_secs();

    snapshot
        .windows
        .iter()
        .filter(|window| {
            window.used_percent >= 100.0
                && window
                    .length
                    .is_some_and(|length| length.as_secs() > candidate_length)
                && same_ladder(snapshot.provider.as_str(), &candidate.key, &window.key)
        })
        .min_by_key(|window| window.length.map(|length| length.as_secs()))
}

fn same_ladder(provider: &str, candidate: &WindowKey, blocker: &WindowKey) -> bool {
    (provider == "kimi" && candidate.as_str() == "rate/w18000" && blocker.as_str() == "w604800")
        || pool(candidate) == pool(blocker)
}

/// The prefix before the final duration key is a provider-supplied quota-pool identity.
/// Flat duration keys are the provider's one default pool.
fn pool(key: &WindowKey) -> Option<&str> {
    key.as_str().rsplit_once("/w").map(|(pool, _)| pool)
}
