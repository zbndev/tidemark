//! Amp.
//!
//! Ported from CodexBar's Swift `Providers/Amp/` — `AmpUsageFetcher.swift` for the
//! request, `AmpUsageParser.swift` and `AmpUsageSnapshot.swift` for the meaning; the
//! parser tests are the contract. Never seen answering: every number in the tests is a
//! body CodexBar recorded.
//!
//! # A POST, not the GET the plan sketch said
//!
//! The one key-authenticated request CodexBar makes is `POST
//! ampcode.com/api/internal?userDisplayBalanceInfo` with the fixed body
//! `{"method":"userDisplayBalanceInfo","params":{}}` and a bearer key; the GET in the
//! same source fetches the settings page as HTML with a browser session cookie, which is
//! the login flow and out of scope. This port sends the POST.
//!
//! # The usage arrives as prose
//!
//! The endpoint answers `{ok, result: {displayText}, error}`, and the display text is
//! not JSON but Amp's own line format, ANSI escapes and trailing links included. The
//! parser walks it line by line:
//!
//! - `Signed in as <email> (<org>)?` — an account row.
//! - `Amp Free: $<remaining>/$<quota> remaining (replenishes +$<rate>/hour)?` — the
//!   dollar form. Used is quota minus remaining; the window length is
//!   `round(quota / rate)` hours (at least one), and the reset is when the allowance
//!   refills: now plus `used / rate` hours. A missing replenishment clause leaves no
//!   length and no reset.
//! - `Amp Free: <n>% remaining (today)? ((resets daily))?` — the percent form, used
//!   only when no dollar line exists. The quota is 100, the window is 24 hours, and
//!   only the literal `(resets daily)` earns a reset: the next 20:00 in America/New
//!   York, computed here from the US DST rules (second Sunday of March to first Sunday
//!   of November), which the recorded fixtures pin across the summer, the boundary
//!   itself, and the winter.
//! - `Subscription <plan>: <n>% other usage and <n>% orb usage remaining - resets upon
//!   renewal in <n> days|months` — two windows of one length (CodexBar's monthly
//!   sentinel, 30 days) against different quotas, keyed by pool `other`/`orb` so both
//!   draw. Renewal adds calendar months — day-clamped, as Swift's calendar does — or
//!   plain days.
//! - `Individual credits: $<n> remaining` and `Workspace <name>: $<n> remaining` —
//!   balances with no limit: rows in a Credits section, never windows.
//!
//! A body that recognises no usage line at all is `Malformed`; the source's signed-out
//! text ("sign in", "login") maps to `Credential` instead. `ok: false` with
//! `auth-required` is the rejection that arrives as a 200: also `Credential`, so the
//! interface asks for a new key rather than patience with an unreadable body. Any other
//! `ok: false` is `Malformed` carrying the provider's own message.
//!
//! # Where this port is stricter than the source
//!
//! CodexBar's regexes skip an `Amp Free:` line they cannot read; this port refuses the
//! whole fetch for one, per the workspace rule that a recognised entry is never a silent
//! absence. A line of an unrecognised kind is still skipped. The signed-out branch and
//! the no-usage-at-all branch have no recorded body on the API path, so no test covers
//! them; the source's own HTML-path fixtures for those shapes are not ported.

use super::{Auth, Method, Spec};
use crate::providers::ProviderError;
use serde::Deserialize;
use tidemark_types::{
    AccountId, DetailRow, DetailSection, ProviderId, Snapshot, Timestamp, Window, WindowKey,
    WindowLength,
};
use time::{Date, Month, OffsetDateTime, UtcOffset};

/// The slug this provider's history is filed under. Never changes once shipped.
pub const PROVIDER_ID: &str = "amp";

/// The endpoint CodexBar's own test pins, query string included.
const USAGE_URL: &str = "https://ampcode.com/api/internal?userDisplayBalanceInfo";

/// The body of the balance RPC, sent verbatim.
const USAGE_BODY: &str = "{\"method\":\"userDisplayBalanceInfo\",\"params\":{}}";

/// The subscription windows' length: CodexBar's `monthlyWindowSentinelMinutes`
/// (30 × 24 × 60), the length it draws a renewal-bounded pool under.
const MONTHLY_SENTINEL_SECS: u64 = 30 * 24 * 60 * 60;

/// The free tier's daily boundary: 20:00 America/New_York, as CodexBar's calendar
/// computes it. Pinned by the recorded fixtures across both DST regimes.
const FREE_TIER_BOUNDARY_HOUR: u8 = 20;

#[derive(Debug, Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Option<ResultBody>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct ResultBody {
    #[serde(default, rename = "displayText")]
    display_text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// One `Amp Free:` reading, in either of the two shapes the display text carries.
#[derive(Debug)]
struct FreeUsage {
    /// Remaining, as the line spells it (dollars or percent).
    remaining: f64,
    /// The allowance (dollars) or 100 (percent form).
    quota: f64,
    /// Dollars per hour; zero when the line states none.
    hourly_replenishment: f64,
    /// Hours, when a length is derivable; the percent form fixes 24.
    window_hours: Option<f64>,
    /// The line carried the literal `(resets daily)`.
    resets_daily: bool,
    /// The line carried dollars, and so has absolutes to show.
    is_dollars: bool,
}

impl FreeUsage {
    /// Used: quota minus remaining, floored at zero.
    fn used(&self) -> f64 {
        (self.quota - self.remaining).max(0.0)
    }

    /// The window length, when one is derivable.
    fn length(&self) -> Option<WindowLength> {
        let hours = self.window_hours.filter(|hours| *hours > 0.0)?;
        WindowLength::from_secs((hours * 60.0).round() as u64 * 60)
    }

    /// Used over quota as a percent, clamped at a full bar, as the source computes it.
    fn used_percent(&self) -> f64 {
        let quota = self.quota.max(0.0);
        let used = self.used().max(0.0);
        if quota > 0.0 {
            (used / quota * 100.0).min(100.0)
        } else {
            0.0
        }
    }

    /// The absolutes under the bar, when the line carried dollars. The percent form has
    /// none, and none are invented for it.
    fn absolutes(&self) -> Option<String> {
        self.is_dollars
            .then(|| format!("{} / {}", money(self.used()), money(self.quota)))
    }

    /// The reset instant: the daily New York boundary when the line says
    /// "(resets daily)", else the moment the allowance refills at its hourly rate.
    /// A line with neither earns no reset.
    fn resets_at(&self, now: Timestamp) -> Option<Timestamp> {
        if self.resets_daily {
            return next_free_tier_reset(now);
        }
        if self.quota > 0.0 && self.hourly_replenishment > 0.0 {
            let seconds = self.used() / self.hourly_replenishment * 3600.0;
            return Some(now.saturating_add_seconds(seconds.round() as i64));
        }
        None
    }
}

/// One `Subscription …` line.
#[derive(Debug)]
struct Subscription {
    plan: String,
    other_remaining: f64,
    orb_remaining: f64,
    /// Renewal instant, from `n` days or calendar `n` months after the reading.
    resets_at: Timestamp,
    /// "renews in 1 month" / "renews in 29 days", the source's own spelling.
    reset_description: String,
}

/// The whole display text, as the lines gave it.
#[derive(Debug, Default)]
struct Display {
    email: Option<String>,
    organization: Option<String>,
    free: Option<FreeUsage>,
    subscription: Option<Subscription>,
    individual_credits: Option<f64>,
    workspaces: Vec<(String, f64)>,
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

    if !envelope.ok {
        // The rejection that arrives as a 200: the interface asks for a new key, not for
        // patience with an unreadable body.
        if envelope
            .error
            .as_ref()
            .and_then(|error| error.code.as_deref())
            == Some("auth-required")
        {
            return Err(ProviderError::Credential { status: 401 });
        }
        let message = envelope
            .error
            .and_then(|error| error.message)
            .unwrap_or_else(|| "Amp usage API returned an error.".to_owned());
        return Err(ProviderError::malformed(message));
    }

    let display_text = envelope
        .result
        .and_then(|result| result.display_text)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| ProviderError::malformed("missing Amp usage display text"))?;
    let display = parse_display(&display_text, captured_at)?;

    if display.free.is_none()
        && display.subscription.is_none()
        && display.individual_credits.is_none()
        && display.workspaces.is_empty()
    {
        if looks_signed_out(&display_text) {
            // The source's signed-out text; no recorded body exercises this on the API
            // path, so no test covers it.
            return Err(ProviderError::Credential { status: 401 });
        }
        return Err(ProviderError::malformed("missing Amp usage data"));
    }

    let mut windows = Vec::new();
    if let Some(subscription) = &display.subscription {
        // Two windows of one length against different quotas: a pool each, so both draw.
        let length = WindowLength::from_secs(MONTHLY_SENTINEL_SECS).expect("a fixed span");
        for (pool, title, remaining) in [
            ("other", "Other usage", subscription.other_remaining),
            ("orb", "Orb usage", subscription.orb_remaining),
        ] {
            windows.push(Window {
                key: WindowKey::for_pool(pool, length),
                title: title.to_owned(),
                subtitle: Some(subscription.reset_description.clone()),
                used_percent: remaining_to_used(remaining),
                resets_at: Some(subscription.resets_at),
                length: Some(length),
            });
        }
    }
    if let Some(free) = &display.free {
        let length = free.length();
        let key = match length {
            // The one length the subscription pools already claim needs a pool of its
            // own, or the two windows would file under one key.
            Some(length) if length.as_secs() == MONTHLY_SENTINEL_SECS => {
                WindowKey::for_pool("amp-free", length)
            }
            Some(length) => WindowKey::for_length(length),
            // A window with no stated length has nothing to key on but its own name.
            None => WindowKey::named("amp-free"),
        };
        windows.push(Window {
            key,
            title: "Amp Free".to_owned(),
            subtitle: free.absolutes(),
            used_percent: free.used_percent(),
            resets_at: free.resets_at(captured_at),
            length,
        });
    }

    let mut plan_rows = Vec::new();
    if let Some(subscription) = &display.subscription {
        plan_rows.push(DetailRow {
            label: "Plan".to_owned(),
            value: subscription.plan.clone(),
        });
    }
    if let Some(email) = &display.email {
        let value = match &display.organization {
            Some(organization) => format!("{email} ({organization})"),
            None => email.clone(),
        };
        plan_rows.push(DetailRow {
            label: "Account".to_owned(),
            value,
        });
    }
    let mut details = Vec::new();
    if !plan_rows.is_empty() {
        details.push(DetailSection {
            title: DetailSection::PLAN.to_owned(),
            rows: plan_rows,
        });
    }
    let mut credit_rows = Vec::new();
    if let Some(credits) = display.individual_credits {
        credit_rows.push(DetailRow {
            label: "Individual credits".to_owned(),
            value: money(credits),
        });
    }
    for (name, remaining) in &display.workspaces {
        credit_rows.push(DetailRow {
            label: format!("Workspace {name}"),
            value: money(*remaining),
        });
    }
    if !credit_rows.is_empty() {
        details.push(DetailSection {
            title: "Credits".to_owned(),
            rows: credit_rows,
        });
    }

    Ok(Snapshot {
        provider: ProviderId::new(PROVIDER_ID),
        account: account.clone(),
        captured_at,
        windows,
        details,
    })
}

/// A `61% remaining` figure becomes `39` used, clamped both ways as the source clamps it.
fn remaining_to_used(remaining: f64) -> f64 {
    100.0 - remaining.clamp(0.0, 100.0)
}

/// The display text, line by line, as the source's regexes read it: the first line of
/// each shape wins, and the dollar form of the free tier outranks the percent form.
fn parse_display(text: &str, now: Timestamp) -> Result<Display, ProviderError> {
    let mut display = Display::default();
    let mut percent_reading: Option<FreeUsage> = None;

    for raw in text.lines() {
        let stripped = strip_ansi(raw);
        let line = stripped.trim();
        if let Some((email, organization)) = identity_in(line) {
            display.email.get_or_insert(email);
            display.organization = display.organization.or(organization);
            continue;
        }
        if let Some(rest) = line.strip_prefix("Amp Free:") {
            if let Some(free) = dollar_free(rest) {
                display.free.get_or_insert(free);
            } else if let Some(free) = percent_free(rest) {
                percent_reading.get_or_insert(free);
            } else {
                // A line this parser recognises whose amount it cannot read is not a
                // reading it can silently drop.
                return Err(ProviderError::malformed(
                    "an Amp Free line carries no readable usage",
                ));
            }
            continue;
        }
        if let Some(subscription) = subscription_in(line, now) {
            display.subscription.get_or_insert(subscription);
            continue;
        }
        if let Some(rest) = line.strip_prefix("Individual credits:") {
            let tail = skip_ws(rest).strip_prefix('$').unwrap_or(skip_ws(rest));
            if let Some((remaining, _)) = take_amount(tail) {
                display.individual_credits.get_or_insert(remaining);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("Workspace ")
            && let Some((name, remaining)) = workspace_in(rest)
        {
            display.workspaces.push((name, remaining));
        }
    }

    display.free = display.free.or(percent_reading);
    Ok(display)
}

/// The `Signed in as <email> (<org>)?` line, ANSI-stripped and trimmed.
fn identity_in(line: &str) -> Option<(String, Option<String>)> {
    let rest = line.strip_prefix("Signed in as ")?;
    let mut words = rest.split_whitespace();
    let email = words.next()?;
    if email.contains('(') {
        return None;
    }
    let tail = rest.strip_prefix(email)?.trim_start();
    let organization = tail
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
        .filter(|organization| !organization.is_empty())
        .map(str::to_owned);
    Some((email.to_owned(), organization))
}

/// The dollar form: `$4.71/$10 remaining (replenishes +$0.42/hour)?`, trailing text
/// ignored.
fn dollar_free(rest: &str) -> Option<FreeUsage> {
    let rest = skip_ws(rest).strip_prefix('$').unwrap_or(skip_ws(rest));
    let (remaining, rest) = take_amount(rest)?;
    let rest = skip_ws(rest).strip_prefix('/')?;
    let rest = skip_ws(rest).strip_prefix('$').unwrap_or(skip_ws(rest));
    let (quota, rest) = take_amount(rest)?;
    let rest = skip_ws(rest).strip_prefix("remaining")?;
    let hourly = replenishment_after(rest).unwrap_or(0.0);
    let window_hours = if hourly > 0.0 {
        Some((quota / hourly).round().max(1.0))
    } else {
        None
    };
    Some(FreeUsage {
        remaining,
        quota,
        hourly_replenishment: hourly,
        window_hours,
        resets_daily: false,
        is_dollars: true,
    })
}

/// The `(replenishes +$0.42/hour)` clause, when the line carries one.
fn replenishment_after(rest: &str) -> Option<f64> {
    let tail = skip_ws(rest).strip_prefix("(replenishes")?;
    let tail = skip_ws(tail).strip_prefix('+')?;
    let tail = skip_ws(tail).strip_prefix('$').unwrap_or(tail);
    let (rate, tail) = take_amount(tail)?;
    skip_ws(tail).strip_prefix("/hour)")?;
    Some(rate)
}

/// The percent form: `61% remaining (today)? ((resets daily))?`, trailing text ignored.
fn percent_free(rest: &str) -> Option<FreeUsage> {
    let rest = skip_ws(rest);
    let (remaining, rest) = take_amount(rest)?;
    let rest = skip_ws(rest).strip_prefix('%')?;
    let rest = skip_ws(rest).strip_prefix("remaining")?;
    let rest = skip_ws(rest).strip_prefix("today").unwrap_or(rest);
    let resets_daily = skip_ws(rest).starts_with("(resets daily)");
    Some(FreeUsage {
        remaining,
        quota: 100.0,
        hourly_replenishment: 0.0,
        window_hours: Some(24.0),
        resets_daily,
        is_dollars: false,
    })
}

/// The subscription line, whole and exact: a plan, two remaining percents, a renewal in
/// days or months, and an optional trailing link. The renewal instant is read against
/// the fetch's own clock, as the source reads it against `now`.
fn subscription_in(line: &str, now: Timestamp) -> Option<Subscription> {
    let rest = line.strip_prefix("Subscription ")?;
    let (plan, rest) = rest.split_once(':')?;
    let plan = plan.trim();
    if plan.is_empty() {
        return None;
    }
    let rest = skip_ws(rest);
    let (other, rest) = take_amount(rest)?;
    let rest = skip_ws(rest).strip_prefix("% other usage and")?;
    let rest = skip_ws(rest);
    let (orb, rest) = take_amount(rest)?;
    let rest = skip_ws(rest).strip_prefix("% orb usage remaining - resets upon renewal in")?;
    let rest = skip_ws(rest);
    let (renewal, rest) = take_count(rest)?;
    let rest = skip_ws(rest);
    let months = rest.starts_with("month");
    let rest = if months && rest.starts_with("month") {
        rest.strip_prefix("months").unwrap_or_else(|| &rest[5..])
    } else if rest.starts_with("day") {
        rest.strip_prefix("days").unwrap_or_else(|| &rest[3..])
    } else {
        return None;
    };
    let rest = skip_ws(rest);
    if !rest.is_empty() && !rest.starts_with("- https://") && !rest.starts_with("- http://") {
        return None;
    }
    let resets_at = if months {
        add_calendar_months(now, renewal)
    } else {
        Timestamp::from_unix(now.as_unix().checked_add(renewal.checked_mul(86_400)?)?).ok()?
    };
    let unit = if months { "month" } else { "day" };
    let plural = if renewal == 1 { "" } else { "s" };
    Some(Subscription {
        plan: plan.to_owned(),
        other_remaining: other,
        orb_remaining: orb,
        resets_at,
        reset_description: format!("renews in {renewal} {unit}{plural}"),
    })
}

/// The `Workspace <name>: $<n> remaining` line: the name runs to the first colon, and
/// the remainder must be an amount and the word `remaining`.
fn workspace_in(rest: &str) -> Option<(String, f64)> {
    let (name, tail) = rest.split_once(':')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let tail = skip_ws(tail).strip_prefix('$').unwrap_or(skip_ws(tail));
    let (remaining, tail) = take_amount(tail)?;
    skip_ws(tail)
        .starts_with("remaining")
        .then(|| (name.to_owned(), remaining))
}

/// `months` calendar months after `at`, day-clamped, as Swift's calendar adds months.
fn add_calendar_months(at: Timestamp, months: i64) -> Timestamp {
    let base = OffsetDateTime::from_unix_timestamp(at.as_unix()).expect("a real instant");
    let total = i64::from(base.year()) * 12 + i64::from(base.month() as u8 - 1) + months;
    let year = total.div_euclid(12) as i32;
    let month = Month::try_from((total.rem_euclid(12) + 1) as u8).expect("a real month");
    let day = base.day().min(time::util::days_in_month(month, year));
    let date = Date::from_calendar_date(year, month, day).expect("a real date");
    Timestamp::from_unix(base.replace_date(date).unix_timestamp()).expect("a real instant")
}

/// The next 20:00 America/New_York strictly after `after`, from the US DST rules.
///
/// 20:00 Eastern is 00:00 or 01:00 UTC on the following day, so the only candidates are
/// those two hours over the next few UTC days; each candidate is checked under the
/// offset in force at that instant (EDT from the second Sunday of March 07:00 UTC to
/// the first Sunday of November 06:00 UTC, EST otherwise).
fn next_free_tier_reset(after: Timestamp) -> Option<Timestamp> {
    let start = OffsetDateTime::from_unix_timestamp(after.as_unix()).ok()?;
    let midnight = start.replace_time(time::Time::MIDNIGHT);
    for day in 0..=3i64 {
        for hour in [1, 0] {
            let candidate = midnight
                .checked_add(time::Duration::days(day))?
                .checked_add(time::Duration::hours(hour))?;
            if candidate.unix_timestamp() <= after.as_unix() {
                continue;
            }
            let local = candidate.to_offset(eastern_offset(candidate));
            if local.hour() == FREE_TIER_BOUNDARY_HOUR && local.minute() == 0 {
                return Timestamp::from_unix(candidate.unix_timestamp()).ok();
            }
        }
    }
    None
}

/// The UTC offset of America/New_York at `at`, Eastern always: -4h in DST, -5h out.
fn eastern_offset(at: OffsetDateTime) -> UtcOffset {
    let dst_starts = nth_sunday(at.year(), Month::March, 2)
        .map(|sunday| sunday.with_time(time::Time::from_hms(7, 0, 0).expect("a real time")))
        .map(|at| at.assume_utc());
    let dst_ends = nth_sunday(at.year(), Month::November, 1)
        .map(|sunday| sunday.with_time(time::Time::from_hms(6, 0, 0).expect("a real time")))
        .map(|at| at.assume_utc());
    let in_dst = match (dst_starts, dst_ends) {
        (Some(starts), Some(ends)) => at >= starts && at < ends,
        _ => false,
    };
    if in_dst {
        UtcOffset::from_hms(-4, 0, 0).expect("a real offset")
    } else {
        UtcOffset::from_hms(-5, 0, 0).expect("a real offset")
    }
}

/// The `n`-th Sunday of `month` in `year` — the second Sunday of March, the first of
/// November, the two instants the US changes its clocks.
fn nth_sunday(year: i32, month: Month, n: u8) -> Option<Date> {
    let first_of_month = Date::from_calendar_date(year, month, 1).ok()?;
    let days_until_sunday = (7 - first_of_month.weekday().number_days_from_sunday()) % 7;
    let day = 1 + days_until_sunday + (n - 1) * 7;
    Date::from_calendar_date(year, month, day).ok()
}

/// Removes ANSI CSI sequences (`ESC[2m`, `ESC[0m`), which the recorded display text
/// wraps its identity line in.
fn strip_ansi(line: &str) -> String {
    let mut cleaned = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            cleaned.push(c);
            continue;
        }
        if chars.next() != Some('[') {
            continue;
        }
        for trailing in chars.by_ref() {
            if trailing.is_ascii_alphabetic() {
                break;
            }
        }
    }
    cleaned
}

/// The source's own signed-out markers.
fn looks_signed_out(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["sign in", "log in", "login", "/login", "ampcode.com/login"]
        .iter()
        .any(|marker| lower.contains(marker))
}

/// An amount as the display text spells it: digits first, commas allowed, an optional
/// fraction. Returns the value and the text after it.
fn take_amount(text: &str) -> Option<(f64, &str)> {
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == ',' || c == '.'))
        .unwrap_or(text.len());
    if end == 0 || !text.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let (digits, rest) = text.split_at(end);
    let value: f64 = digits.replace(',', "").parse().ok()?;
    Some((value, rest))
}

/// A whole positive count with commas, as the renewal states it.
fn take_count(text: &str) -> Option<(i64, &str)> {
    let (value, rest) = take_amount(text)?;
    if value.fract() != 0.0 || value <= 0.0 {
        return None;
    }
    Some((value as i64, rest))
}

fn skip_ws(text: &str) -> &str {
    text.trim_start()
}

/// An amount of money in the source's own spelling: `$` and grouping, two decimals.
fn money(value: f64) -> String {
    let rendered = format!("{value:.2}");
    let (int_part, rest) = rendered.split_once('.').unwrap_or((rendered.as_str(), ""));
    let bytes = int_part.as_bytes();
    let mut grouped = String::with_capacity(int_part.len() + bytes.len() / 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(*byte as char);
    }
    if !rest.is_empty() {
        grouped.push('.');
        grouped.push_str(rest);
    }
    format!("${grouped}")
}

/// Amp as the keyed mechanism sees it.
pub static SPEC: Spec = Spec {
    id: PROVIDER_ID,
    title: "Amp",
    endpoint: |_| USAGE_URL.to_owned(),
    method: Method::Post {
        body: USAGE_BODY,
        content_type: "application/json",
    },
    auth: Auth::Bearer,
    headers: &[("Accept", "application/json")],
    parse: parse_for_account,
    credential_hint: "ampcode.com settings → API key (AMP_API_KEY).",
    options: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;
    use tidemark_types::{DetailRow, Snapshot, Window};

    /// Recorded by CodexBar, `AmpUsageParserTests.swift` — "parses current amp usage
    /// display text". ANSI escapes and trailing links included, exactly as recorded.
    /// CodexBar asserts quota 10, used 5.29, hourly replenishment 0.42, window 24 hours,
    /// credits $25.64 and workspace meow $10.22.
    const CURRENT_DISPLAY: &str = "\u{1b}[2mSigned in as ampcode@3kh0.net (echo)\u{1b}[0m\nAmp Free: $4.71/$10 remaining (replenishes +$0.42/hour) - https://ampcode.com/settings#amp-free\nIndividual credits: $25.64 remaining (set up automatic top-up to avoid running out) - https://ampcode.com/settings\nWorkspace meow: $10.22 remaining (set up automatic top-up to avoid running out) - https://ampcode.com/workspaces/meow";

    /// Recorded by CodexBar, same file — "parses percentage based amp free usage".
    /// CodexBar asserts 61% remaining reads as 39 used of a 100 quota over 24 hours,
    /// resetting at 2023-11-15T01:00:00Z.
    const PERCENT_DISPLAY: &str = "Signed in as user@example.com (example)\nAmp Free: 61% remaining today (resets daily) - https://ampcode.com/settings#amp-free\nIndividual credits: $9.86 remaining (set up automatic top-up to avoid running out)\nWorkspace example: $5.33 remaining (set up automatic top-up to avoid running out)";

    /// Recorded by CodexBar, same file — "does not infer daily reset from percentage
    /// alone". No "today", no "(resets daily)": no reset is invented.
    const PERCENT_WITHOUT_DAILY: &str = "Signed in as user@example.com\nAmp Free: 61% remaining";

    /// Recorded by CodexBar, same file — "legacy amp free usage keeps replenishment
    /// reset when percentage text also exists". The dollar form wins over the percent
    /// form, and its reset is now + 8 hours (4 used at 0.5/hour).
    const DOLLAR_AND_PERCENT: &str = "Signed in as user@example.com\nAmp Free: $6/$10 remaining (replenishes +$0.5/hour)\nAmp Free: 61% remaining today (resets daily)";

    /// Recorded by CodexBar, `Fixtures/Providers/Amp/monthly-subscription.txt`, asserted
    /// by "parses monthly subscription fixture and both metered pools".
    const MONTHLY_SUBSCRIPTION: &str = "Signed in as fixture@example.test (example)\nAmp Free: 61% remaining today (resets daily) - https://ampcode.com/settings#amp-free\nSubscription Gigawatt: 73% other usage and 91% orb usage remaining - resets upon renewal in 1 month\nIndividual credits: $17.23 remaining (set up automatic re-load to avoid running out) - https://ampcode.com/settings\nWorkspace meow: $5.33 remaining (set up automatic re-load to avoid running out) - https://ampcode.com/workspaces/meow";

    /// Recorded by CodexBar, `Fixtures/Providers/Amp/day-subscription.txt`, asserted by
    /// "parses day based subscription fixture".
    const DAY_SUBSCRIPTION: &str = "Signed in as fixture@example.test (example)\nSubscription Megawatt: 97% other usage and 100% orb usage remaining - resets upon renewal in 29 days";

    /// Recorded by CodexBar, same file — "parses amp subscription usage with settings
    /// link". The trailing link is part of the recorded line.
    const SUBSCRIPTION_WITH_LINK: &str = "Subscription Megawatt: 97% other usage and 100% orb usage remaining - resets upon renewal in 29 days - https://ampcode.com/settings#subscription";

    /// Recorded by CodexBar, same file — "parses individual credits without free tier
    /// usage". No window at all; the credits are rows.
    const CREDITS_ONLY: &str =
        "Signed in as paid@example.com\nIndividual credits: $25.64 remaining";

    /// Recorded by CodexBar, same file — "parses workspace credits without free tier
    /// usage". Comma-grouped and whole-dollar amounts both appear.
    const WORKSPACES_ONLY: &str = "Signed in as workspace@example.com (team)\nWorkspace Alpha Team: $1,234.56 remaining\nWorkspace Beta: $7 remaining";

    /// Recorded by CodexBar, same file — "parses current usage api response". This is
    /// the shape the endpoint itself answers: the display text wrapped in the
    /// `ok`/`result` envelope. CodexBar asserts used 2, credits $12.50, both workspaces.
    const API_RESPONSE: &str = "{\"ok\":true,\"result\":{\"displayText\":\"Signed in as user@example.com (team)\\nAmp Free: $8/$10 remaining (replenishes +$0.5/hour)\\nIndividual credits: $12.50 remaining\\nWorkspace Alpha Team: $1,234.56 remaining\\nWorkspace Beta: $7 remaining\"}}";

    /// Recorded by CodexBar, same file — "usage api auth error is invalid API token".
    /// The rejection arrives in the body of a successful response.
    const REJECTED_KEY: &str =
        "{\"ok\":false,\"error\":{\"code\":\"auth-required\",\"message\":\"Sign in\"}}";

    fn at(unix: i64) -> Timestamp {
        Timestamp::from_unix(unix).expect("plausible")
    }

    /// The recorded envelope shape (`parses current usage api response`) around a
    /// recorded display text, so each parser-level fixture reaches `parse` the way the
    /// endpoint would deliver it.
    fn envelope(text: &str) -> String {
        serde_json::json!({"ok": true, "result": {"displayText": text}}).to_string()
    }

    fn window<'a>(snapshot: &'a Snapshot, key: &str) -> &'a Window {
        snapshot
            .windows
            .iter()
            .find(|w| w.key.as_str() == key)
            .unwrap_or_else(|| panic!("no window {key} in {:?}", snapshot.windows))
    }

    fn row<'a>(snapshot: &'a Snapshot, in_section: &str, label: &str) -> &'a DetailRow {
        let found = snapshot
            .details
            .iter()
            .find(|section| section.title == in_section)
            .unwrap_or_else(|| panic!("no section {in_section} in {:?}", snapshot.details));
        found
            .rows
            .iter()
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("no row {label} in {in_section}"))
    }

    #[test]
    fn the_current_display_fixture_draws_the_free_window_and_the_credits() {
        let snapshot = parse(&envelope(CURRENT_DISPLAY), at(1_700_000_000)).expect("parses");
        assert_eq!(snapshot.provider.as_str(), PROVIDER_ID);

        let free = window(&snapshot, "w86400");
        assert_eq!(free.title, "Amp Free");
        assert!(
            (free.used_percent - 52.9).abs() < 0.001,
            "$4.71 of $10 remaining reads as 5.29 used: {}",
            free.used_percent
        );
        assert_eq!(free.subtitle.as_deref(), Some("$5.29 / $10.00"));
        assert_eq!(
            free.length.expect("round(10 / 0.42) is 24 hours").as_secs(),
            86_400
        );

        assert_eq!(
            row(&snapshot, "Credits", "Individual credits").value,
            "$25.64",
            "the numbers are grouped the way CodexBar groups them"
        );
        assert_eq!(row(&snapshot, "Credits", "Workspace meow").value, "$10.22");
        assert_eq!(
            row(&snapshot, "Plan", "Account").value,
            "ampcode@3kh0.net (echo)",
            "the ANSI escapes are stripped before the identity is read"
        );
    }

    #[test]
    fn the_percent_fixture_reads_39_used_and_resets_at_the_new_york_boundary() {
        let snapshot = parse(&envelope(PERCENT_DISPLAY), at(1_700_000_000)).expect("parses");
        let free = window(&snapshot, "w86400");
        assert_eq!(free.used_percent, 39.0);
        assert_eq!(
            free.subtitle, None,
            "a percent-only line carries no absolutes"
        );
        assert_eq!(
            free.resets_at,
            Some(at(1_700_010_000)),
            "2023-11-15T01:00:00Z — 20:00 EST, the instant CodexBar's own test reads"
        );

        let no_daily = parse(&envelope(PERCENT_WITHOUT_DAILY), at(1_700_000_000)).expect("parses");
        assert_eq!(window(&no_daily, "w86400").used_percent, 39.0);
        assert_eq!(
            window(&no_daily, "w86400").resets_at,
            None,
            "without \"(resets daily)\" no reset is inferred"
        );
    }

    #[test]
    fn the_dollar_form_wins_and_resets_when_the_allowance_refills() {
        let snapshot = parse(&envelope(DOLLAR_AND_PERCENT), at(1_700_000_000)).expect("parses");
        let free = window(&snapshot, "w72000");
        assert_eq!(
            free.used_percent, 40.0,
            "$6 of $10 remaining reads as 4 used"
        );
        assert_eq!(
            free.length.expect("round(10 / 0.5) is 20 hours").as_secs(),
            72_000
        );
        assert_eq!(
            free.resets_at,
            Some(at(1_700_028_800)),
            "now + 4 used at 0.5/hour = 8 hours, CodexBar's own assertion"
        );
    }

    #[test]
    fn the_monthly_fixture_draws_both_subscription_pools_and_the_free_window() {
        let snapshot = parse(&envelope(MONTHLY_SUBSCRIPTION), at(1_785_794_400)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(
            keys,
            ["other/w2592000", "orb/w2592000", "w86400"],
            "two windows of one length against different quotas take a pool each"
        );

        let other = window(&snapshot, "other/w2592000");
        assert_eq!(other.title, "Other usage");
        assert_eq!(other.used_percent, 27.0, "73% remaining reads as 27 used");
        assert_eq!(other.subtitle.as_deref(), Some("renews in 1 month"));
        assert_eq!(
            other.resets_at,
            Some(at(1_788_472_800)),
            "2026-09-03T22:00:00Z — one calendar month on, CodexBar's own assertion"
        );
        assert_eq!(
            other.length.expect("the monthly sentinel").as_secs(),
            2_592_000
        );

        let orb = window(&snapshot, "orb/w2592000");
        assert_eq!(orb.title, "Orb usage");
        assert_eq!(orb.used_percent, 9.0);
        assert_eq!(orb.resets_at, Some(at(1_788_472_800)));

        let free = window(&snapshot, "w86400");
        assert_eq!(free.used_percent, 39.0);
        assert_eq!(
            free.resets_at,
            Some(at(1_785_801_600)),
            "2026-08-04T00:00:00Z — 20:00 EDT, CodexBar's own assertion"
        );

        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Gigawatt");
        assert_eq!(
            row(&snapshot, "Plan", "Account").value,
            "fixture@example.test (example)"
        );
        assert_eq!(
            row(&snapshot, "Credits", "Individual credits").value,
            "$17.23"
        );
        assert_eq!(row(&snapshot, "Credits", "Workspace meow").value, "$5.33");
    }

    #[test]
    fn the_day_fixture_renews_twenty_nine_days_on() {
        let snapshot = parse(&envelope(DAY_SUBSCRIPTION), at(1_700_000_000)).expect("parses");
        let keys: Vec<&str> = snapshot.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["other/w2592000", "orb/w2592000"]);
        let other = window(&snapshot, "other/w2592000");
        assert_eq!(other.used_percent, 3.0, "97% remaining reads as 3 used");
        assert_eq!(other.subtitle.as_deref(), Some("renews in 29 days"));
        assert_eq!(
            other.resets_at,
            Some(at(1_702_505_600)),
            "now + 29 days, CodexBar's own assertion"
        );
        assert_eq!(window(&snapshot, "orb/w2592000").used_percent, 0.0);
        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Megawatt");
    }

    #[test]
    fn a_subscription_line_may_carry_a_trailing_link() {
        let snapshot = parse(&envelope(SUBSCRIPTION_WITH_LINK), at(1_700_000_000)).expect("parses");
        assert_eq!(window(&snapshot, "other/w2592000").used_percent, 3.0);
        assert_eq!(row(&snapshot, "Plan", "Plan").value, "Megawatt");
    }

    #[test]
    fn credits_without_free_usage_are_rows_not_windows() {
        let snapshot = parse(&envelope(CREDITS_ONLY), at(1_700_000_000)).expect("parses");
        assert!(
            snapshot.windows.is_empty(),
            "no free line and no subscription: nothing to draw"
        );
        assert_eq!(
            row(&snapshot, "Credits", "Individual credits").value,
            "$25.64"
        );
        assert_eq!(row(&snapshot, "Plan", "Account").value, "paid@example.com");

        let workspaces = parse(&envelope(WORKSPACES_ONLY), at(1_700_000_000)).expect("parses");
        assert!(workspaces.windows.is_empty());
        assert_eq!(
            row(&workspaces, "Credits", "Workspace Alpha Team").value,
            "$1,234.56"
        );
        assert_eq!(row(&workspaces, "Credits", "Workspace Beta").value, "$7.00");
    }

    #[test]
    fn the_api_envelope_fixture_parses_like_its_display_text() {
        let snapshot = parse(API_RESPONSE, at(1_700_005_000)).expect("parses");
        let free = window(&snapshot, "w72000");
        assert_eq!(
            free.used_percent, 20.0,
            "$8 of $10 remaining reads as 2 used"
        );
        assert_eq!(free.subtitle.as_deref(), Some("$2.00 / $10.00"));
        assert_eq!(
            free.resets_at,
            Some(at(1_700_019_400)),
            "now + 2 used at 0.5/hour = 4 hours"
        );
        assert_eq!(
            row(&snapshot, "Credits", "Individual credits").value,
            "$12.50"
        );
        assert_eq!(
            row(&snapshot, "Credits", "Workspace Alpha Team").value,
            "$1,234.56"
        );
        assert_eq!(row(&snapshot, "Credits", "Workspace Beta").value, "$7.00");
        assert_eq!(
            row(&snapshot, "Plan", "Account").value,
            "user@example.com (team)"
        );
    }

    #[test]
    fn the_free_tier_daily_reset_observes_new_york_daylight_saving_time() {
        // CodexBar's own three instants for the recorded monthly fixture: the summer
        // boundary one second early, the boundary itself, and the winter boundary.
        let boundary = |captured_at: i64| {
            parse(&envelope(MONTHLY_SUBSCRIPTION), at(captured_at))
                .expect("parses")
                .windows
                .iter()
                .find(|w| w.key.as_str() == "w86400")
                .expect("the free window")
                .resets_at
                .expect("resets")
                .as_unix()
        };
        assert_eq!(
            boundary(1_785_801_599),
            1_785_801_600,
            "2026-08-04T00:00:00Z"
        );
        assert_eq!(
            boundary(1_785_801_600),
            1_785_888_000,
            "2026-08-05T00:00:00Z, the boundary itself rolls to the next day"
        );
        assert_eq!(
            boundary(1_768_525_199),
            1_768_525_200,
            "2026-01-16T01:00:00Z, EST"
        );
    }

    #[test]
    fn a_rejected_key_in_a_200_body_asks_for_a_new_key() {
        let error = parse(REJECTED_KEY, at(1_700_000_000)).expect_err("rejected");
        assert!(
            matches!(error, ProviderError::Credential { status: 401 }),
            "{error}"
        );
    }

    #[test]
    fn bodies_we_cannot_read_are_refused_wholesale() {
        let other_error = "{\"ok\":false,\"error\":{\"code\":\"other\",\"message\":\"boom\"}}";
        let ok_without_text = "{\"ok\":true}";
        let unreadable_amount = API_RESPONSE.replace("$8", "$eight");
        for body in [
            "not json",
            "{\"partial\":",
            other_error,
            ok_without_text,
            unreadable_amount.as_str(),
        ] {
            let error = parse(body, at(1_700_000_000))
                .expect_err("a body this shape fails the whole fetch");
            assert!(
                matches!(error, ProviderError::Malformed(_)),
                "{error} for {body}"
            );
        }
    }

    #[test]
    fn a_line_of_an_unrecognised_kind_is_skipped_and_an_unreadable_one_refused() {
        let with_unknown = format!(
            "Signed in as user@example.com\nSurprise fund: $5 remaining\n{}",
            PERCENT_WITHOUT_DAILY
        );
        let snapshot = parse(&envelope(&with_unknown), at(1_700_000_000)).expect("parses");
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].key.as_str(), "w86400");

        let unreadable = "Signed in as user@example.com\nAmp Free: $six/$10 remaining\nIndividual credits: $5 remaining";
        assert!(
            matches!(
                parse(&envelope(unreadable), at(1_700_000_000)),
                Err(ProviderError::Malformed(_))
            ),
            "an Amp Free line whose amount is a word is a reading this port cannot make"
        );
    }

    #[test]
    fn the_spec_posts_the_internal_rpc_with_a_bearer_key() {
        use crate::providers::keyed::{Auth, Method, Options};
        assert_eq!(SPEC.id, PROVIDER_ID);
        assert_eq!(SPEC.title, "Amp");
        assert_eq!(
            (SPEC.endpoint)(&Options::new()),
            "https://ampcode.com/api/internal?userDisplayBalanceInfo"
        );
        assert_eq!(SPEC.auth, Auth::Bearer);
        assert_eq!(
            SPEC.method,
            Method::Post {
                body: "{\"method\":\"userDisplayBalanceInfo\",\"params\":{}}",
                content_type: "application/json",
            }
        );
        assert!(
            SPEC.headers.contains(&("Accept", "application/json")),
            "the recorded request carries this header"
        );
        assert!(SPEC.options.is_empty(), "Amp has nothing to choose");
    }
}
