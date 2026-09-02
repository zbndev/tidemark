//! Z.ai / GLM.
//!
//! Two `GET`s, a bearer token, no OAuth, no local CLI, no cookies. The first request is
//! the quota meter; the second reads the wallet the same key can see. It was the simplest
//! provider in the catalog — one `GET`, describable by a [`super::Spec`] — and the wallet
//! is what ended that: a second request means a [`super::HandSpec`] and an `impl Provider`
//! of its own, like OpenRouter's credits-plus-quota before it.
//!
//! # What the quota payload does not tell you
//!
//! `{code, msg, success, data:{limits[], level}}`, where each limit is
//! `{type, unit, number, percentage, nextResetTime}` plus optional absolutes. Two things
//! in there are not derivable from the payload:
//!
//! 1. **`unit` is an enum with no names on the wire** — `{1: day, 3: hour, 5: minute,
//!    6: week}`, and window length is `number × unit`. Nothing in the response says so.
//! 2. **`TIME_LIMIT` with `unit=5, number=1` is the monthly MCP pool**, not a one-minute
//!    window. By the table above it computes to sixty seconds, which would put the pace
//!    mark at 100% within a minute of every reset and make a month-long quota read as
//!    permanently exhausted. There is no field that distinguishes it; it is a hardcoded
//!    special case, carried over from the reference implementation and confirmed against
//!    the live account, where that limit reports a 1000-call pool resetting in three
//!    weeks.
//!
//! 3. **`nextResetTime` is dropped entirely by a window that has just reset**, observed
//!    live: two hours after the five-hour window rolled over, and with nothing spent in
//!    the new one, the entry arrived as `{type, unit, number, percentage: 0}` and nothing
//!    else. The field comes back once the window is in use. The length is still derivable,
//!    so the window is still drawn — it just has no pace mark until then, and since the
//!    five-hour window is the one the card leads with, that is a routine state rather
//!    than an edge case. See `CONTEXT.md` § Vocabulary on Pace.
//!
//! `nextResetTime` is Unix **milliseconds**.
//!
//! # The second request: the wallet
//!
//! `GET /api/biz/account/query-customer-account-report` — the console's own billing call,
//! and it accepts the same bearer key the quota does (verified live against `api.z.ai`,
//! `open.bigmodel.cn` and `www.bigmodel.cn`; CodexBar ships it CN-only with a comment
//! saying the global platform has no equivalent, which no longer holds). `data` carries
//! `{balance, availableBalance, rechargeAmount, giveAmount, totalSpendAmount,
//! frozenBalance}` and, on every account this has been observed on, `creditBalance` /
//! `availableCreditBalance` arrive `null` beside `creditStatus: "NOT_OPEN"` — the credit
//! ledger is not represented here, because its open state has never been seen and an
//! unknown shape is not to be invented.
//!
//! Three things about that body:
//!
//! 1. **The amounts are Java `BigDecimal`s.** Zeros arrive as `0E-9` and `0.000000` —
//!    valid JSON numbers, if strange ones, and `f64` reads both. The fixtures below keep
//!    the spellings verbatim so the quirk stays covered.
//! 2. **There is no currency field.** The global platform bills in USD through Stripe and
//!    the China one in CNY, so the symbol is chosen by region — a choice, not a reading.
//! 3. **The request is best-effort.** The quota is the point of the fetch; a wallet that
//!    will not answer costs the balance and nothing else. Every failure — transport,
//!    status, unreadable body — is swallowed into silence, never into a failed fetch, and
//!    never into a credential error the successful quota request has already disproved.
//!
//! # What the card does with a wallet
//!
//! While there is money on the account, overage bills the wallet and the MCP pool stops
//! being the binding constraint, so its window yields its row and its absolutes stay in
//! the details, where they always were; the wallet itself is not a window at all — money
//! is not a rate, so there is no honest percentage or bar — it is the first row under
//! [`DetailSection::BALANCE`], which the card lifts as a bold amount beside the quota
//! rows. At zero — spent out, never topped up, or unreadable — nothing about money is
//! published at all and the MCP window returns: a wallet with nothing in it is not
//! something to show, on the card or anywhere else. The rule is on `availableBalance`,
//! resolved as `availableBalance` when the provider sends it and `balance` otherwise,
//! and `frozenBalance` aside they agree.

use super::{HandSpec, OptionSchema, Options};
use crate::providers::{BoxFuture, Credential, Provider, ProviderError, http};
use serde::Deserialize;
use std::fmt;
use std::sync::Arc;
use tidemark_types::{
    AccountId, CredentialKind, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window,
    WindowKey, WindowLength,
};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "zai";

/// Path appended to the region's base URL for the quota meter.
const QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";

/// Path appended to the region's base URL for the wallet reading.
const BALANCE_PATH: &str = "/api/biz/account/query-customer-account-report";

/// Which deployment the account lives on. The two are the same API on different hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Region {
    /// `api.z.ai`.
    #[default]
    Global,
    /// `open.bigmodel.cn`.
    BigModelCn,
}

impl Region {
    /// Base URL for this region.
    pub fn base_url(self) -> &'static str {
        match self {
            Self::Global => "https://api.z.ai",
            Self::BigModelCn => "https://open.bigmodel.cn",
        }
    }

    /// The currency symbol the region bills in. The wallet body carries no currency of
    /// its own; this is the platform's pricing currency, not a reading.
    pub fn currency(self) -> &'static str {
        match self {
            Self::Global => "$",
            Self::BigModelCn => "¥",
        }
    }

    /// The value this region is stored as in `config.toml`.
    pub fn as_value(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::BigModelCn => "bigmodel-cn",
        }
    }

    /// The region a stored value names. An unrecognised value is the default rather than
    /// an error: a typo in `config.toml` must not take the account off the air.
    pub fn from_value(raw: Option<&str>) -> Self {
        match raw {
            Some("bigmodel-cn") => Self::BigModelCn,
            _ => Self::Global,
        }
    }
}

/// Name of the region setting under `[provider.zai]`.
pub const REGION: &str = "region";

/// Z.ai as the settings dialog sees it. A [`HandSpec`] rather than a [`super::Spec`]
/// because the fetch is two requests, and a `Spec` states that it is one.
pub static SPEC: HandSpec = HandSpec {
    id: PROVIDER_ID,
    title: "Z.ai",
    credential: CredentialKind::Key,
    credential_hint: "Z.ai dashboard → API keys, on whichever region your account is on.",
    options: &[OptionSchema {
        name: REGION,
        title: "Region",
        description: Some(
            "The same API on two hosts. A key issued for one is rejected by the other.",
        ),
        default: "global",
        choices: &[
            ("global", "Global (api.z.ai)"),
            ("bigmodel-cn", "China (open.bigmodel.cn)"),
        ],
        required: false,
    }],
    build,
};

/// Builds a pollable client from the stored key and the account's settings.
fn build(
    account: AccountId,
    credential: Credential,
    options: &Options,
) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(Arc::new(Zai::new_for_account(
        account, credential, options,
    )?))
}

/// One Z.ai account: the key, the region it lives on, and the two URLs both decide.
pub struct Zai {
    tidemark_account: AccountId,
    client: reqwest::Client,
    credential: Credential,
    region: Region,
    quota_url: String,
    balance_url: String,
}

impl Zai {
    /// Builds a client for the default account.
    pub fn new(credential: Credential, options: &Options) -> Result<Self, ProviderError> {
        Self::new_for_account(AccountId::default(), credential, options)
    }

    /// Builds a client. Both URLs are resolved once, here, because the region is part of
    /// each path's host and a setting that changed it would otherwise take effect only on
    /// the next daemon restart.
    fn new_for_account(
        account: AccountId,
        credential: Credential,
        options: &Options,
    ) -> Result<Self, ProviderError> {
        let region = Region::from_value(options.get(REGION).map(String::as_str));
        Self::at(account, credential, region, region.base_url())
    }

    /// Builds a client against an explicit base. The production constructor's other half,
    /// and the test suite's door to a loopback server.
    fn at(
        account: AccountId,
        credential: Credential,
        region: Region,
        base: &str,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            tidemark_account: account,
            client: http::client()?,
            credential,
            region,
            quota_url: format!("{base}{QUOTA_PATH}"),
            balance_url: format!("{base}{BALANCE_PATH}"),
        })
    }

    /// The quota URL this instance polls.
    pub fn quota_url(&self) -> &str {
        &self.quota_url
    }

    /// The wallet URL this instance polls.
    pub fn balance_url(&self) -> &str {
        &self.balance_url
    }

    /// The quota request, built but not sent, so the shape is testable without a server.
    fn quota_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.get(&self.quota_url)
    }

    /// The wallet request, likewise.
    fn balance_request(&self) -> Result<reqwest::Request, ProviderError> {
        self.get(&self.balance_url)
    }

    fn get(&self, url: &str) -> Result<reqwest::Request, ProviderError> {
        self.client
            .get(url)
            .bearer_auth(self.credential.expose())
            .header(reqwest::header::ACCEPT, "application/json")
            .build()
            .map_err(|error| ProviderError::Client(super::redact_query(error)))
    }

    async fn fetch_inner(&self) -> Result<Snapshot, ProviderError> {
        if self.credential.is_blank() {
            return Err(ProviderError::Credential { status: 401 });
        }
        let now = Timestamp::now();
        let quota = parse_for_account(
            &super::request(PROVIDER_ID, &self.client, self.quota_request()?).await?,
            now,
            &self.tidemark_account,
        )?;
        // Best-effort, deliberately: the quota is the point of the fetch, and a wallet
        // that will not answer must not cost the card its windows — nor flip a working
        // key into a credential error the quota request has already disproved.
        let wallet = self.wallet().await.ok();
        Ok(apply_balance(quota, wallet.as_ref(), self.region))
    }

    /// Fetches and reads the wallet body.
    async fn wallet(&self) -> Result<WalletAmounts, ProviderError> {
        let body = super::request(PROVIDER_ID, &self.client, self.balance_request()?).await?;
        parse_wallet(&body)
    }
}

impl fmt::Debug for Zai {
    /// Written by hand: a derived impl would print the credential the first time anything
    /// traced a client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Zai")
            .field("id", &PROVIDER_ID)
            .field("quota_url", &self.quota_url)
            .field("balance_url", &self.balance_url)
            .finish_non_exhaustive()
    }
}

impl Provider for Zai {
    fn id(&self) -> ProviderId {
        ProviderId::new(PROVIDER_ID)
    }

    fn account(&self) -> AccountId {
        self.tidemark_account.clone()
    }

    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
        Box::pin(self.fetch_inner())
    }
}

/// Turns a response body into a snapshot. Pure: every trap above is reachable from a test.
pub fn parse(body: &str, captured_at: Timestamp) -> Result<Snapshot, ProviderError> {
    parse_for_account(body, captured_at, &AccountId::default())
}

pub fn parse_for_account(
    body: &str,
    captured_at: Timestamp,
    account: &AccountId,
) -> Result<Snapshot, ProviderError> {
    let envelope: Envelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected envelope: {e}")))?;

    if !envelope.success || envelope.code != 200 {
        let msg = envelope.msg.unwrap_or_else(|| "no message".to_owned());
        return Err(ProviderError::malformed(format!(
            "provider reported failure: code {} — {msg}",
            envelope.code
        )));
    }
    let data = envelope
        .data
        .ok_or_else(|| ProviderError::malformed("successful response carried no data"))?;

    // Recognise the kind *before* deserializing the entry, so that a quota type invented
    // after this was written can carry any shape it likes without breaking the ones we do
    // understand. Once a kind is recognised, a shape we cannot read is an error.
    let mut limits = Vec::new();
    for entry in data.limits {
        let Some(kind) = entry
            .get("type")
            .and_then(serde_json::Value::as_str)
            .and_then(Kind::recognise)
        else {
            continue;
        };
        let limit: Limit = serde_json::from_value(entry).map_err(|e| {
            ProviderError::malformed(format!("{kind:?} limit entry is not readable: {e}"))
        })?;
        limits.push(Parsed::new(kind, limit));
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account.clone(),
        captured_at,
        windows: limits.iter().map(Parsed::window).collect(),
        details: details(&limits, data.level.as_deref()),
    })
}

/// The limit kinds this parser understands.
///
/// Anything else is skipped rather than refused: an unfamiliar `type` is a quota kind that
/// did not exist when this was written. A kind we *do* know that then fails to parse is a
/// different matter and fails the whole fetch — see the module docs on `providers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Token allowance. The main pool on legacy plans.
    Tokens,
    /// Credit allowance. The main pool on current plans.
    Credit,
    /// MCP tool calls. A different pool, not a different length of the same one.
    Time,
}

impl Kind {
    fn recognise(raw: &str) -> Option<Self> {
        match raw {
            "TOKENS_LIMIT" => Some(Self::Tokens),
            "CREDIT_LIMIT" => Some(Self::Credit),
            "TIME_LIMIT" => Some(Self::Time),
            _ => None,
        }
    }
}

/// Seconds in one of the `unit` enum's values, and the noun to call it.
fn unit(code: i64) -> Option<(u64, &'static str)> {
    match code {
        1 => Some((86_400, "day")),
        3 => Some((3_600, "hour")),
        5 => Some((60, "minute")),
        6 => Some((604_800, "week")),
        _ => None,
    }
}

/// Thirty days. What the MCP marker actually means.
const MCP_WINDOW_SECS: u64 = 30 * 86_400;

/// One limit, with the meaning applied.
#[derive(Debug)]
struct Parsed {
    kind: Kind,
    raw: Limit,
    length: Option<WindowLength>,
    used_percent: f64,
    resets_at: Option<Timestamp>,
    title: String,
}

impl Parsed {
    fn new(kind: Kind, raw: Limit) -> Self {
        let is_mcp_marker = kind == Kind::Time && raw.unit == 5 && raw.number == 1;
        let length = if is_mcp_marker {
            WindowLength::from_secs(MCP_WINDOW_SECS)
        } else {
            unit(raw.unit)
                .filter(|_| raw.number > 0)
                .and_then(|(secs, _)| WindowLength::from_secs(secs * raw.number as u64))
        };
        let title = if is_mcp_marker {
            "MCP".to_owned()
        } else {
            match unit(raw.unit) {
                Some((_, noun)) if raw.number > 0 => {
                    let plural = if raw.number == 1 { "" } else { "s" };
                    format!("{} {noun}{plural}", raw.number)
                }
                // The provider described the window in terms we do not have a name for.
                // Better an honest placeholder than a confident wrong one.
                _ => "Quota".to_owned(),
            }
        };
        Self {
            used_percent: used_percent(&raw),
            // An absurd reset time is dropped, not fatal: the window is still real and
            // still worth drawing, it just loses its pace mark. Providers have been
            // observed reporting 1970.
            resets_at: raw
                .next_reset_time
                .and_then(|ms| Timestamp::from_unix_millis(ms).ok()),
            length,
            title,
            kind,
            raw,
        }
    }

    fn key(&self) -> WindowKey {
        match (self.kind, self.length) {
            // MCP calls draw on their own pool. Keyed by pool as well as length so that a
            // future token window of the same length cannot collide with it.
            (Kind::Time, Some(length)) => WindowKey::for_pool("mcp", length),
            (_, Some(length)) => WindowKey::for_length(length),
            // No derivable length, so no length to key on. The raw descriptors are at
            // least stable between responses, which is all a key has to be.
            (_, None) => WindowKey::named(&format!("zai-u{}n{}", self.raw.unit, self.raw.number)),
        }
    }

    fn window(&self) -> Window {
        Window {
            key: self.key(),
            title: self.title.clone(),
            subtitle: None,
            used_percent: self.used_percent,
            resets_at: self.resets_at,
            length: self.length,
        }
    }

    /// The `label: value` row this limit contributes, when it reports absolutes at all.
    fn detail_row(&self) -> Option<DetailRow> {
        let usage = self.raw.usage?;
        let used = absolute_used(&self.raw)?;
        let label = match self.kind {
            Kind::Tokens => format!("{} tokens", self.title),
            Kind::Credit => format!("{} credits", self.title),
            Kind::Time => format!("{} calls", self.title),
        };
        let mut value = format!("{used} of {usage} used");
        if let Some(remaining) = self.raw.remaining {
            value.push_str(&format!(" · {remaining} left"));
        }
        Some(DetailRow { label, value })
    }
}

/// Consumption, preferring absolutes over the reported percentage.
///
/// `percentage` is an integer, so a thousand-call pool reads 0% until ten calls are spent.
/// Where the provider also sends absolutes they are strictly better, and the bar moves when
/// the quota moves.
fn used_percent(raw: &Limit) -> f64 {
    let reported = raw.percentage.clamp(0.0, 100.0);
    let Some(usage) = raw.usage.filter(|u| *u > 0) else {
        return reported;
    };
    let Some(used) = absolute_used(raw) else {
        return reported;
    };
    (used.clamp(0, usage) as f64 * 100.0 / usage as f64).clamp(0.0, 100.0)
}

/// How much of the pool is spent, in the pool's own units.
fn absolute_used(raw: &Limit) -> Option<i64> {
    match (raw.remaining, raw.current_value) {
        (Some(remaining), Some(current)) => Some((raw.usage? - remaining).max(current)),
        (Some(remaining), None) => Some(raw.usage? - remaining),
        (None, Some(current)) => Some(current),
        (None, None) => None,
    }
}

fn details(limits: &[Parsed], level: Option<&str>) -> Vec<DetailSection> {
    let mut sections = Vec::new();

    if let Some(level) = level.map(str::trim).filter(|l| !l.is_empty()) {
        sections.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: vec![DetailRow {
                label: "Level".to_owned(),
                value: level.to_owned(),
            }],
        });
    }

    let rows: Vec<DetailRow> = limits.iter().filter_map(Parsed::detail_row).collect();
    if !rows.is_empty() {
        sections.push(DetailSection {
            title: "Quota".to_owned(),
            rows,
        });
    }

    let per_model: Vec<DetailRow> = limits
        .iter()
        .filter(|l| l.kind == Kind::Time)
        .flat_map(|l| l.raw.usage_details.iter().flatten())
        .filter_map(|detail| {
            Some(DetailRow {
                label: detail.model_code.clone()?,
                value: detail.usage?.to_string(),
            })
        })
        .collect();
    if !per_model.is_empty() {
        sections.push(DetailSection {
            title: "MCP tools".to_owned(),
            rows: per_model,
        });
    }

    sections
}

/// The wallet amounts, resolved.
#[derive(Debug, Clone, PartialEq)]
struct WalletAmounts {
    /// What can still be spent. `availableBalance` where the provider sends it, `balance`
    /// otherwise — they differ only by the frozen part.
    available: f64,
    /// Ever topped up with money.
    recharged: f64,
    /// Ever granted for free.
    granted: f64,
    /// Ever spent.
    spent: f64,
    /// Held back by the provider.
    frozen: f64,
}

/// Reads the wallet body. Pure: the BigDecimal spellings and the absent-amount case are
/// reachable from a test.
fn parse_wallet(body: &str) -> Result<WalletAmounts, ProviderError> {
    #[derive(Deserialize)]
    struct BalanceEnvelope {
        code: i64,
        #[serde(default)]
        msg: Option<String>,
        #[serde(default)]
        success: bool,
        #[serde(default)]
        data: Option<BalanceData>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct BalanceData {
        #[serde(default)]
        balance: Option<f64>,
        #[serde(default)]
        available_balance: Option<f64>,
        #[serde(default)]
        recharge_amount: Option<f64>,
        #[serde(default)]
        give_amount: Option<f64>,
        #[serde(default)]
        total_spend_amount: Option<f64>,
        #[serde(default)]
        frozen_balance: Option<f64>,
    }

    let envelope: BalanceEnvelope = serde_json::from_str(body)
        .map_err(|e| ProviderError::malformed(format!("not the expected wallet envelope: {e}")))?;
    if !envelope.success || envelope.code != 200 {
        let msg = envelope.msg.unwrap_or_else(|| "no message".to_owned());
        return Err(ProviderError::malformed(format!(
            "the wallet reported failure: code {} — {msg}",
            envelope.code
        )));
    }
    let data = envelope
        .data
        .ok_or_else(|| ProviderError::malformed("the wallet response carried no data"))?;
    let readable = |value: Option<f64>| value.filter(|v| v.is_finite());
    let available = readable(data.available_balance.or(data.balance)).ok_or_else(|| {
        ProviderError::malformed("the wallet carried no readable balance to show")
    })?;
    Ok(WalletAmounts {
        available,
        recharged: readable(data.recharge_amount).unwrap_or(0.0),
        granted: readable(data.give_amount).unwrap_or(0.0),
        spent: readable(data.total_spend_amount).unwrap_or(0.0),
        frozen: readable(data.frozen_balance).unwrap_or(0.0),
    })
}

/// Folds the wallet into a quota snapshot. Pure: every card rule in the module docs is
/// reachable from a test.
///
/// `None` is a wallet that could not be read: nothing is gated, nothing is invented, and
/// the details say so.
fn apply_balance(
    mut snapshot: Snapshot,
    wallet: Option<&WalletAmounts>,
    region: Region,
) -> Snapshot {
    // Only a wallet with money in it is published at all. While there is some, overage
    // bills the wallet and the MCP pool stops being the binding constraint, so its window
    // yields its row — its absolutes stay in the details, where they always were — and the
    // wallet becomes the first BALANCE detail row, which the card lifts as a bold amount;
    // see `DetailSection::BALANCE`. At zero, spent out, or unreadable there is no balance
    // to show anywhere: no window, no section, and the MCP window returns. The card lifts
    // the first BALANCE row verbatim and never parses an amount, so whether one is
    // publishable is this function's decision to make.
    if let Some(amounts) = wallet.filter(|amounts| amounts.available > 0.0) {
        snapshot
            .windows
            .retain(|window| !window.key.as_str().starts_with("mcp/"));
        insert_section(&mut snapshot, wallet_details(amounts, region));
    }
    snapshot
}

/// The full amounts, for the detail dialog.
fn wallet_details(amounts: &WalletAmounts, region: Region) -> DetailSection {
    let mut rows = vec![labeled("Prepaid balance", money(region, amounts.available))];
    // A zero is not a fact about the account worth a row; a non-zero one is.
    for (label, value) in [
        ("Topped up", amounts.recharged),
        ("Granted", amounts.granted),
        ("Frozen", amounts.frozen),
        ("Spent", amounts.spent),
    ] {
        if value > 0.0 {
            rows.push(labeled(label, money(region, value)));
        }
    }
    DetailSection {
        title: DetailSection::BALANCE.to_owned(),
        rows,
    }
}

/// Puts a section after the plan, when there is one, and ahead of everything else.
fn insert_section(snapshot: &mut Snapshot, section: DetailSection) {
    let at = snapshot
        .details
        .iter()
        .position(|existing| existing.title != DetailSection::PLAN)
        .unwrap_or(snapshot.details.len());
    snapshot.details.insert(at, section);
}

fn labeled(label: &str, value: impl ToString) -> DetailRow {
    DetailRow {
        label: label.to_owned(),
        value: value.to_string(),
    }
}

/// An amount in the region's billing currency, two fraction digits.
fn money(region: Region, amount: f64) -> String {
    format!("{}{amount:.2}", region.currency())
}

#[derive(Debug, Deserialize)]
struct Envelope {
    code: i64,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<Data>,
}

#[derive(Debug, Deserialize)]
struct Data {
    #[serde(default)]
    limits: Vec<serde_json::Value>,
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Limit {
    unit: i64,
    number: i64,
    percentage: f64,
    #[serde(default)]
    usage: Option<i64>,
    #[serde(default)]
    current_value: Option<i64>,
    #[serde(default)]
    remaining: Option<i64>,
    #[serde(default)]
    next_reset_time: Option<i64>,
    #[serde(default)]
    usage_details: Option<Vec<UsageDetail>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageDetail {
    #[serde(default)]
    model_code: Option<String>,
    #[serde(default)]
    usage: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// Every quota fixture below is hand-authored. It reproduces the *shape* of a live
    /// response — which is a fact about the API — with invented numbers, because the
    /// values are the user's real consumption and this repository is going to be public.
    const LIVE_SHAPE: &str = r#"{
      "code": 200,
      "msg": "Operation successful",
      "data": {
        "limits": [
          {"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":40,
           "remaining":960,"percentage":4,"nextResetTime":1789122642999,
           "usageDetails":[{"modelCode":"web-reader","usage":25},{"modelCode":"zread","usage":15}]},
          {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":12,"nextResetTime":1787164114706},
          {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":37,"nextResetTime":1787221842997}
        ],
        "level": "pro"
      },
      "success": true
    }"#;

    /// The wallet bodies, likewise: the shape and the BigDecimal spellings (`0E-9`,
    /// `0.000000`, the null credit ledger) are the live API's, recorded 2026-09-02, and
    /// the amounts are invented — a coherent story rather than a balance: 25.00 granted,
    /// nothing topped up, 18.75 spent, 6.25 left.
    const WALLET_WITH_MONEY: &str = r#"{
      "code": 200,
      "msg": "Operation successful",
      "data": {
        "balance": 6.25,
        "rechargeAmount": 0.000000,
        "giveAmount": 25.000000,
        "totalSpendAmount": 18.75,
        "todaySpendAmount": null,
        "availableBalance": 6.25,
        "frozenBalance": 0E-9,
        "creditBalance": null,
        "availableCreditBalance": null,
        "creditStatus": "NOT_OPEN",
        "modelSpendAmountList": null,
        "isKA": false
      },
      "success": true
    }"#;

    /// The same body as the provider sends an account that has never held money: every
    /// amount a zero, in each of the two zero spellings.
    const WALLET_EMPTY: &str = r#"{
      "code": 200,
      "msg": "Operation successful",
      "data": {
        "balance": 0E-9,
        "rechargeAmount": 0.000000,
        "giveAmount": 0.000000,
        "totalSpendAmount": 0E-9,
        "todaySpendAmount": null,
        "availableBalance": 0E-9,
        "frozenBalance": 0E-9,
        "creditBalance": null,
        "availableCreditBalance": null,
        "creditStatus": "NOT_OPEN",
        "modelSpendAmountList": null,
        "isKA": false
      },
      "success": true
    }"#;

    fn now() -> Timestamp {
        Timestamp::from_unix(1_787_000_000).expect("plausible")
    }

    fn parsed(body: &str) -> Snapshot {
        parse(body, now()).expect("parses")
    }

    fn one_limit(fields: &str) -> Snapshot {
        parsed(&format!(
            r#"{{"code":200,"success":true,"data":{{"limits":[{fields}]}}}}"#
        ))
    }

    fn find<'a>(snapshot: &'a Snapshot, key: &str) -> &'a Window {
        snapshot
            .windows
            .iter()
            .find(|w| w.key.as_str() == key)
            .unwrap_or_else(|| panic!("no window {key} in {:?}", snapshot.windows))
    }

    fn keys(snapshot: &Snapshot) -> Vec<&str> {
        let mut keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        keys.sort_unstable();
        keys
    }

    fn with_wallet(body: &str) -> Snapshot {
        let wallet = parse_wallet(body).expect("parses");
        apply_balance(parsed(LIVE_SHAPE), Some(&wallet), Region::Global)
    }

    #[test]
    fn one_response_carries_three_windows_of_three_lengths() {
        let snapshot = parsed(LIVE_SHAPE);
        assert_eq!(keys(&snapshot), ["mcp/w2592000", "w18000", "w604800"]);
        assert_eq!(snapshot.provider.as_str(), "zai");
        assert_eq!(snapshot.captured_at, now());
    }

    #[test]
    fn every_window_in_a_snapshot_has_its_own_key() {
        // Two windows sharing a key would land on the same storage row, and the second one
        // would be silently reported stale rather than stored.
        let snapshot = parsed(LIVE_SHAPE);
        let mut keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        keys.sort_unstable();
        let unique = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), unique);
    }

    #[test]
    fn the_mcp_marker_is_a_month_and_not_a_minute() {
        // unit=5, number=1 computes to sixty seconds by the unit table. Taking that
        // literally would put the pace mark at 100% a minute after every reset.
        let snapshot = parsed(LIVE_SHAPE);
        let mcp = find(&snapshot, "mcp/w2592000");
        assert_eq!(mcp.length, WindowLength::from_secs(30 * 86_400));
        assert_eq!(mcp.title, "MCP");
        assert!(
            mcp.pace(now()).expect("computable") < 0.9,
            "a month-long window should not read as nearly elapsed"
        );
    }

    #[test]
    fn a_one_minute_token_window_is_still_taken_literally() {
        // The special case is the MCP *kind*, not the numbers. Nothing else gets it.
        let snapshot = one_limit(r#"{"type":"TOKENS_LIMIT","unit":5,"number":1,"percentage":3}"#);
        assert_eq!(snapshot.windows[0].length, WindowLength::from_secs(60));
        assert_eq!(snapshot.windows[0].title, "1 minute");
    }

    #[test]
    fn lengths_come_from_the_unnamed_unit_enum() {
        let snapshot = parsed(LIVE_SHAPE);
        assert_eq!(
            find(&snapshot, "w18000").length,
            WindowLength::from_secs(5 * 3_600)
        );
        assert_eq!(find(&snapshot, "w18000").title, "5 hours");
        assert_eq!(find(&snapshot, "w604800").title, "1 week");
    }

    #[test]
    fn reset_times_arrive_in_milliseconds() {
        let snapshot = parsed(LIVE_SHAPE);
        let five_hour = find(&snapshot, "w18000");
        assert_eq!(
            five_hour.resets_at,
            Some(Timestamp::from_unix(1_787_164_114).expect("plausible"))
        );
    }

    #[test]
    fn an_absurd_reset_time_costs_the_pace_mark_not_the_window() {
        let snapshot = one_limit(
            r#"{"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":50,"nextResetTime":0}"#,
        );
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].resets_at, None);
        assert_eq!(snapshot.windows[0].used_percent, 50.0);
    }

    #[test]
    fn a_window_that_just_reset_omits_its_reset_time_and_keeps_its_length() {
        // Observed live: two hours after the five-hour window rolled over, with nothing
        // spent in the new one, the entry carried no `nextResetTime` at all.
        let snapshot = one_limit(r#"{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":0}"#);
        let window = &snapshot.windows[0];
        assert_eq!(window.length, WindowLength::from_secs(18_000));
        assert_eq!(window.key.as_str(), "w18000");
        assert_eq!(window.resets_at, None);
        assert_eq!(window.pace(now()), None, "no reset time means no pace mark");
        assert_eq!(window.is_outpacing(now()), None);
    }

    #[test]
    fn absolutes_beat_the_integer_percentage() {
        // A thousand-call pool reads 0% for its first ten calls if you trust `percentage`.
        let snapshot = one_limit(
            r#"{"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":5,
                "remaining":995,"percentage":0}"#,
        );
        assert!((snapshot.windows[0].used_percent - 0.5).abs() < 1e-9);
    }

    #[test]
    fn the_larger_of_the_two_spent_counts_wins() {
        // `usage - remaining` and `currentValue` disagree; the provider's own client takes
        // the larger, and under-reporting consumption is the dangerous direction.
        let snapshot = one_limit(
            r#"{"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":25,
                "remaining":990,"percentage":1}"#,
        );
        assert!((snapshot.windows[0].used_percent - 2.5).abs() < 1e-9);
    }

    #[test]
    fn without_absolutes_the_reported_percentage_stands() {
        let snapshot = one_limit(r#"{"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":89}"#);
        assert_eq!(snapshot.windows[0].used_percent, 89.0);
    }

    #[test]
    fn an_unfamiliar_quota_kind_is_skipped_rather_than_refused() {
        let snapshot = parsed(
            r#"{"code":200,"success":true,"data":{"limits":[
                 {"type":"MYSTERY_LIMIT","shape":{"we":"have","never":"seen"}},
                 {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":10}
               ]}}"#,
        );
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].key.as_str(), "w604800");
    }

    #[test]
    fn a_familiar_kind_we_cannot_read_fails_the_whole_fetch() {
        // The alternative is dropping the window, and a missing window reads as "you have
        // no such limit" — the most dangerous thing this program can say.
        let err = parse(
            r#"{"code":200,"success":true,"data":{"limits":[
                 {"type":"TOKENS_LIMIT","unit":"hour","number":5,"percentage":10}
               ]}}"#,
            now(),
        )
        .expect_err("must refuse");
        assert!(matches!(err, ProviderError::Malformed(_)), "{err:?}");
    }

    #[test]
    fn an_unknown_unit_still_draws_a_window_just_without_a_pace_mark() {
        let snapshot = one_limit(r#"{"type":"TOKENS_LIMIT","unit":9,"number":3,"percentage":42}"#);
        assert_eq!(snapshot.windows[0].length, None);
        assert_eq!(snapshot.windows[0].key.as_str(), "zai-u9n3");
        assert_eq!(snapshot.windows[0].used_percent, 42.0);
        assert_eq!(snapshot.windows[0].pace(now()), None);
    }

    #[test]
    fn a_failed_envelope_is_never_read_as_data() {
        let err = parse(
            r#"{"code":401,"msg":"invalid api key","success":false,"data":null}"#,
            now(),
        )
        .expect_err("must refuse");
        assert!(format!("{err}").contains("invalid api key"), "{err}");
    }

    #[test]
    fn a_success_flag_without_data_is_refused() {
        assert!(parse(r#"{"code":200,"success":true}"#, now()).is_err());
    }

    #[test]
    fn details_carry_the_plan_the_absolutes_and_the_per_tool_counts() {
        let snapshot = parsed(LIVE_SHAPE);
        let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Plan", "Quota", "MCP tools"]);
        assert_eq!(snapshot.details[0].rows[0].value, "pro");
        assert_eq!(snapshot.details[1].rows[0].label, "MCP calls");
        assert_eq!(
            snapshot.details[1].rows[0].value,
            "40 of 1000 used · 960 left"
        );
        assert_eq!(snapshot.details[2].rows.len(), 2);
        assert_eq!(snapshot.details[2].rows[0].label, "web-reader");
    }

    #[test]
    fn a_response_with_nothing_to_say_produces_no_empty_sections() {
        let snapshot = one_limit(r#"{"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":10}"#);
        assert!(snapshot.details.is_empty());
    }

    #[test]
    fn the_card_leads_with_the_five_hour_window() {
        let dominant = parsed(LIVE_SHAPE)
            .dominant_window()
            .expect("present")
            .clone();
        assert_eq!(dominant.key.as_str(), "w18000");
    }

    #[test]
    fn the_wallet_readings_parse_with_their_bigdecimal_spellings() {
        let wallet = parse_wallet(WALLET_WITH_MONEY).expect("parses");
        assert!((wallet.available - 6.25).abs() < 1e-9);
        assert!((wallet.granted - 25.0).abs() < 1e-9);
        assert!((wallet.spent - 18.75).abs() < 1e-9);
        assert_eq!(wallet.recharged, 0.0);

        // The empty wallet's zeros are `0E-9` and `0.000000` on the wire — both must read
        // as zero, not as a parse failure or as something invented.
        let empty = parse_wallet(WALLET_EMPTY).expect("parses");
        assert_eq!(empty.available, 0.0);
        assert_eq!(empty.spent, 0.0);
    }

    #[test]
    fn a_wallet_without_a_readable_amount_is_refused() {
        // The fetch treats a refusal as "no wallet read": gating on an invented zero would
        // be the dangerous direction, and so would showing one.
        for body in [
            r#"{"code":200,"success":true,"data":{"balance":null,"availableBalance":null}}"#,
            r#"{"code":200,"success":true,"data":{}}"#,
            r#"{"code":200,"success":true}"#,
            r#"{"code":500,"msg":"系统异常","success":false,"data":null}"#,
            "not json",
        ] {
            let error = parse_wallet(body).expect_err("must refuse");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{body}: {error:?}"
            );
        }
    }

    #[test]
    fn a_wallet_with_money_moves_mcp_off_the_card_and_reports_the_amount() {
        let snapshot = with_wallet(WALLET_WITH_MONEY);

        // The card draws its quota rows and lifts the first BALANCE row as a bold amount;
        // the MCP window yields its row, and its absolutes were always detail rows and
        // stay.
        assert_eq!(keys(&snapshot), ["w18000", "w604800"]);
        assert!(
            !keys(&snapshot).contains(&"balance"),
            "money is not a rate window"
        );

        let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Plan", "Balance", "Quota", "MCP tools"]);
        let rows = &snapshot.details[1].rows;
        assert_eq!(rows[0].label, "Prepaid balance");
        assert_eq!(rows[0].value, "$6.25");
        assert_eq!(rows[1].label, "Granted");
        assert_eq!(rows[1].value, "$25.00");
        assert_eq!(rows[2].label, "Spent");
        assert_eq!(rows[2].value, "$18.75");
        assert_eq!(
            rows.len(),
            3,
            "a zero top-up and a zero freeze are not rows"
        );
        assert_eq!(
            snapshot.details[2].rows[0].value, "40 of 1000 used · 960 left",
            "the MCP absolutes stay in the details"
        );
    }

    #[test]
    fn an_empty_wallet_keeps_the_mcp_window_and_publishes_nothing_about_money() {
        // At zero the MCP pool is all there is: its window returns, and nothing about the
        // wallet is published — not a window, not a section. A card lifts the first
        // BALANCE row verbatim, so a "$0.00" row there would put a bold zero on the card.
        let snapshot = with_wallet(WALLET_EMPTY);
        assert_eq!(keys(&snapshot), ["mcp/w2592000", "w18000", "w604800"]);
        let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Plan", "Quota", "MCP tools"]);
    }

    #[test]
    fn a_spent_out_wallet_keeps_the_mcp_window_and_publishes_nothing_about_money() {
        // Spent to the last cent, the wallet binds no more than an empty one does: the MCP
        // window returns and there is no balance anywhere.
        let mut wallet = parse_wallet(WALLET_WITH_MONEY).expect("parses");
        wallet.available = 0.0;
        let snapshot = apply_balance(parsed(LIVE_SHAPE), Some(&wallet), Region::Global);
        let keys = keys(&snapshot);
        assert!(!keys.contains(&"balance"));
        assert!(keys.contains(&"mcp/w2592000"));
        let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Plan", "Quota", "MCP tools"]);
    }

    #[test]
    fn a_wallet_that_cannot_be_read_costs_neither_the_windows_nor_the_quota() {
        let snapshot = apply_balance(parsed(LIVE_SHAPE), None, Region::Global);
        assert_eq!(keys(&snapshot), ["mcp/w2592000", "w18000", "w604800"]);
        let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Plan", "Quota", "MCP tools"]);
    }

    #[test]
    fn the_wallet_amounts_carry_the_regions_currency() {
        let wallet = parse_wallet(WALLET_WITH_MONEY).expect("parses");
        let snapshot = apply_balance(parsed(LIVE_SHAPE), Some(&wallet), Region::BigModelCn);
        assert_eq!(snapshot.details[1].rows[0].value, "¥6.25");
    }

    #[test]
    fn the_five_hour_window_keeps_the_lead_whatever_the_wallet_says() {
        let snapshot = with_wallet(WALLET_WITH_MONEY);
        assert_eq!(
            snapshot.dominant_window().expect("present").key.as_str(),
            "w18000"
        );
    }

    #[test]
    fn both_urls_carry_the_region() {
        for (stored, base) in [
            (None, "https://api.z.ai"),
            (Some("bigmodel-cn"), "https://open.bigmodel.cn"),
        ] {
            let options = stored
                .map(|value: &str| [(REGION.to_owned(), value.to_owned())])
                .unwrap_or_default()
                .into_iter()
                .collect::<Options>();
            let zai = Zai::new(Credential::new("fixture-key"), &options).expect("builds");
            assert_eq!(zai.quota_url(), format!("{base}{QUOTA_PATH}"));
            assert_eq!(zai.balance_url(), format!("{base}{BALANCE_PATH}"));
        }
    }

    #[test]
    fn both_requests_carry_the_bearer_key_and_the_shared_shape() {
        let zai = Zai::new(Credential::new("fixture-key"), &Options::new()).expect("builds");
        for request in [zai.quota_request(), zai.balance_request()] {
            let request = request.expect("builds");
            assert_eq!(request.method(), reqwest::Method::GET);
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .expect("present"),
                "Bearer fixture-key"
            );
            assert_eq!(
                request
                    .headers()
                    .get(reqwest::header::ACCEPT)
                    .expect("present"),
                "application/json"
            );
        }
    }

    #[test]
    fn an_unknown_region_falls_back_to_global_rather_than_refusing_to_poll() {
        let options: Options = [("region".to_owned(), "atlantis".to_owned())]
            .into_iter()
            .collect();
        let zai = Zai::new(Credential::new("key"), &options).expect("builds");
        assert_eq!(
            zai.quota_url(),
            "https://api.z.ai/api/monitor/usage/quota/limit",
            "a typo in config.toml must not take the account off the air"
        );
    }

    #[test]
    fn the_region_option_is_published_with_both_hosts() {
        let region = SPEC
            .options
            .iter()
            .find(|option| option.name == REGION)
            .expect("the region is published");
        let values: Vec<&str> = region.choices.iter().map(|(value, _)| *value).collect();
        assert_eq!(values, ["global", "bigmodel-cn"]);
        assert_eq!(region.default, "global");
    }

    #[test]
    fn the_spec_builds_a_client_the_registry_can_poll() {
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "Z.ai");
        assert_eq!(SPEC.credential, CredentialKind::Key);
        let provider = build(
            AccountId::default(),
            Credential::new("key"),
            &Options::new(),
        )
        .expect("no required options, so no refusal");
        assert_eq!(provider.id().as_str(), "zai");
        assert_eq!(provider.account().as_str(), "default");
    }

    #[test]
    fn a_zai_client_never_prints_its_credential() {
        let zai = Zai::new(Credential::new("sk-super-secret"), &Options::new()).expect("builds");
        let rendered = format!("{zai:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }

    /// A loopback server answering the two requests a fetch makes: the quota path with
    /// `quota_body`, the wallet path with `wallet_status`/`wallet_body`. Both exchanges
    /// are captured for the assertions that follow.
    fn two_request_server(
        quota_body: &'static str,
        wallet_status: u16,
        wallet_body: &'static str,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("request accepted");
                let mut reader = BufReader::new(&mut stream);
                let mut head = String::new();
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("header read");
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().expect("content length");
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0; content_length];
                if content_length > 0 {
                    reader.read_exact(&mut body).expect("body read");
                }
                let is_quota = head.starts_with("GET /api/monitor");
                tx.send(head).expect("request captured");
                let (status, body) = if is_quota {
                    ("200 OK", quota_body)
                } else {
                    match wallet_status {
                        200 => ("200 OK", wallet_body),
                        _ => ("500 Internal Server Error", wallet_body),
                    }
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("response written");
            }
        });
        (format!("http://{address}"), rx)
    }

    fn at(base: &str) -> Zai {
        Zai::at(
            AccountId::default(),
            Credential::new("zai-fixture-key"),
            Region::Global,
            base,
        )
        .expect("builds")
    }

    #[tokio::test]
    async fn a_fetch_reads_both_endpoints_and_gates_the_mcp_window() {
        let (base, requests) = two_request_server(LIVE_SHAPE, 200, WALLET_WITH_MONEY);
        let snapshot = at(&base).fetch().await.expect("both requests answer");
        assert_eq!(keys(&snapshot), ["w18000", "w604800"]);
        assert_eq!(snapshot.details[1].rows[0].value, "$6.25");

        let first = requests.recv().expect("quota request captured");
        let second = requests.recv().expect("wallet request captured");
        for (path, head) in [
            ("/api/monitor/usage/quota/limit", first),
            ("/api/biz/account/query-customer-account-report", second),
        ] {
            assert!(head.starts_with(&format!("GET {path} ")), "{head}");
            assert!(
                head.contains("authorization: Bearer zai-fixture-key"),
                "{head}"
            );
        }
    }

    #[tokio::test]
    async fn a_wallet_that_fails_costs_only_the_balance_row() {
        // The quota is the point of the fetch: a 500 on the wallet must not fail it, gate
        // the MCP window on a balance nobody read, or turn a working key into a credential
        // error the quota already disproved.
        let (base, _requests) = two_request_server(LIVE_SHAPE, 500, "internal");
        let snapshot = at(&base).fetch().await.expect("the quota still answers");
        assert_eq!(keys(&snapshot), ["mcp/w2592000", "w18000", "w604800"]);
        let titles: Vec<&str> = snapshot.details.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Plan", "Quota", "MCP tools"],
            "no balance section either"
        );
    }
}
