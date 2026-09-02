# Multi-Account Providers — Design

> **Status:** Approved design, pre-implementation.
> **Branch:** `feat/multi-account-providers` — all work lands here, never directly on `main`.
> **Scope:** API-key and OAuth providers only. Credential-free (`None`) and external-session
> (`External`, e.g. Antigravity's `agy` read) providers stay single-account.

## What this is
One provider, several sets of credentials — a work Claude and a personal Claude, two Z.ai
keys. Today Tidemark builds exactly one `Account` per configured provider, hard-wired to
`AccountId::default()` at every construction site, even though `AccountId` already flows
through secrets, history, the D-Bus wire shape and the tray. This feature makes account
construction plural and gives the UI a way to create, name, show and order the accounts.

The load-bearing observation: **the plumbing is already per-account.** Secrets are keyed
`(Kind, ProviderId, AccountId)` (`secrets.rs::attributes`); every history table keys on
`(provider, account, window, …)` (`storage/mod.rs`, with the test
`accounts_of_the_same_provider_do_not_share_a_segment_counter` already proving isolation);
`ProviderStatus.account` is on the wire; the tray already renders a shared provider as
`Claude (work)` (`tray.rs::label`). What is missing is (a) config that can name more than
one account, (b) registry/engine code that builds them, (c) one D-Bus method to add and
order them, and (d) the UI.

## Invariants

- A provider's account list always contains `"default"`, and `"default"` is always first.
  The UI never special-cases a missing main account.
- Removing the main account promotes the next one: the daemon rewrites the account list so
  the survivor becomes `"default"`. No other account's id, credentials or history change.
- Account ids are stable storage keys (config, Secret Service, history, D-Bus). They are
  lowercase slugs, unique within their provider. Renaming changes the display label and,
  when confirmed, migrates the id (§ Rename).
- Extra OAuth accounts are **Tidemark-login only**. The vendor-CLI credential source is
  exclusive to the default account — the CLI file is one login on disk, so an extra account
  has no file of its own to read.
- Presentation invents nothing: a collapsed main card shows only its own account's numbers;
  accounts are never aggregated into a reading no provider reported.
- Removed accounts keep their history rows (the existing removal dialog already promises
  "Quota history will be kept").

## Config

`providers = [...]` stays a flat provider array — it is the card-reorder contract
(`SetOrder` is validated as an exact permutation of it) and does not change shape. Accounts
become a per-provider ordered list:

```toml
providers = ["claude", "zai"]

[provider.claude]
accounts = ["default", "work"]
```

`Config::providers()` is untouched. A new `Config::accounts(provider)` returns the ordered
list, defaulting to `["default"]` when the key is absent — existing config files are
unchanged and mean exactly what they mean today. Writes go through the same
edit-in-place, malformed-config-errors rules as every other key.

## Daemon

### Registry & engine

- `registry::accounts` expands each configured provider into one `Account` per entry of
  `Config::accounts(provider)`, in order. The per-account constructors
  (`keyed_account`, `claude_account`, `codex_account`, `hand_written_account`) take the
  `AccountId` and pass it to `Account::new` / `Provider::account()` instead of
  `AccountId::default()`.
- `Account::with_client` derives its account from `client.account()` already; the OAuth
  clients (`Claude`, `Codex`) currently hard-code `AccountId::default()` in
  `Provider::account()` and in the `Secrets` lookup — they take the id at construction and
  use it for both.
- The engine's `accounts` vector is already ordered and polled independently per entry;
  no scheduler change. `Engine::add_provider` gains an account parameter (today it
  duplicates-checks on `AccountId::default()`); `Engine::remove_provider` already targets
  an exact `(provider, account)` pair and is unchanged, plus the promotion rewrite when the
  removed account was `"default"`.
- New `Engine::set_account_order(provider, accounts)` rewrites one provider's `accounts`
  array and re-sorts the in-memory vector, mirroring `set_order`.

### D-Bus surface

Already account-addressed and unchanged: `set_key`, `sign_out`, `begin_login`,
`await_login`, `cancel_login`, `remove_provider`, `GetStatus`, `ProviderChanged`,
`Publication::Removed`. All take/emit `(provider, account)`.

Additive:

- **`AddAccount(provider: s, account: s) -> ()`** — validates the id (well-formed slug,
  unique within the provider, provider supports multi-account), persists to
  `accounts = [...]`, constructs the `Account`, publishes a pending `ProviderStatus`.
  Goes through the `Command` queue like `AddProvider`.
- **`SetAccountOrder(provider: s, accounts: as) -> ()`** — validates an exact permutation
  of that provider's account list and persists it.
- **`ProviderStatus.account_label: Option<String>`** — new key in the `a{sv}` dict.
  `None`/absent for the default account; `Some(label)` for extras. Because the dict is
  extensible and absent-means-absent, an old daemon simply omits it and the UI falls back
  to the account id.

`AddProvider` (called from the picker) keeps adding the `"default"` account.

## Provider-settings dialog

### Rows

`ConfiguredList` currently renders one flat `adw::ActionRow` per status. It now groups by
provider: the provider's row first, then one indented row per extra account, in `accounts`
order. Each row keeps its own pen (edit) and trash (remove) buttons, operating on that
`(provider, account)`.

The account row's title is the provider title (fixed); beside it an **editable account
label** (an inline `adw::EntryRow`-style text the user can change) — per your direction,
the name lives next to the uneditable provider name, not in a separate dialog.

### The "+" button

Each row for a multi-account-capable provider (credential kind `Key` or `OAuth`) gets a
**"+" button to the left of the pen** (`list-add-symbolic`, tooltip "Add account"). It
opens a small `adw::AlertDialog` with one entry row for the account name. On confirm:

1. The name is slugified to the account id (lowercased, non-alphanumeric → `-`).
2. `AddAccount(provider, id)` is called.
3. The existing detail sub-page opens for the new account — the *same* authentication UI
   as today: `build_key_rows` for a key provider, the sign-in row for OAuth.
4. OAuth waits reuse the existing pending-login flow (the row shows "Waiting for your
   browser…"); nothing new is built for the wait.

### Rename

The display label is derived from the account id, and the id is the storage key — so a
rename is a **migrate**, the one genuinely risky operation. Editing the label in place and
confirming performs, atomically at the daemon:

1. Validate the new id (well-formed, unique within the provider).
2. Copy the Secret Service slot to the new id (`get` old → `set` new → `delete` old).
3. Re-key history: `UPDATE … SET account = ?new WHERE provider = ? AND account = ?old`
   across the `window_state`, `segment`, `point` and notice tables in one transaction.
4. Rewrite the provider's `accounts` array, preserving position.
5. Reload the account.

If any step fails the whole rename is refused and the old id is left intact (secret copy
and history re-key are both reversible up to the final `delete`). If this proves
controversial in review it can be deferred behind "no rename; delete and re-add" — but it
is specced because the label is presented as editable.

## Main window

### Grouping

`MainWindow::show_all`/`show_one` group statuses by `status.provider`. The first account
(`"default"`) is the group's main card; the rest are its extra cards.

### The expand button

A provider with more than one account renders an expand control on its main card: a flat
circular button with a `pan-down-symbolic`/`pan-up-symbolic` icon plus a **"+N" count**
label, placed at the end of the title row after the state chip. The title row's existing
ellipsize rules (mark/name/plan/chip all ellipsize, name last to lose characters) absorb
the tighter space; the card's `MIN_WIDTH` does not change.

- **Collapsed** (the default, and the state on every window open): the main card shows only
  its own account's numbers. No aggregation, no worst-case tint — the count is the only
  signal that more accounts exist.
- **Expanded**: the extra accounts are inserted directly after the main card as **full
  cards**, each titled by its account label. Second click collapses them away.
- Expand state is **in-memory only**. A group auto-expands when the user finishes adding an
  account to it, so the new card is visible.

### The group backdrop

Per your correction: cards are not individually darkened. The `CardGrid` draws a **shared
darker rounded-rectangle backdrop under the contiguous block of cells an expanded group
occupies**, painted in the grid's `snapshot()` before the cards. This is a layout change,
not a restyle:

- The grid computes every cell's rect in `size_allocate` already; the backdrop is the
  union of the group's cells, drawn as a rounded region behind them.
- A group can wrap across rows (the grid is 1–3 columns by width), so the backdrop follows
  the cells wherever they land rather than assuming one row.
- A card mid-drag interpolates between slots; the backdrop stays at the resting layout and
  the dragged card lifts off it, which is the intended look.
- Collapsed and single-account providers occupy one cell and draw no backdrop — visually
  identical to today.

The backdrop gets a CSS class (`group-bin`) with a slightly darker fill and marginally
tighter internal spacing so the grouped cards read as belonging together.

### Reorder

You chose groups + intra-group. The grid's drag gesture reports `(from, to)` slot indices;
the window's reorder handler maps them back:

- **Across a group boundary** → provider reorder → existing `SetOrder(Vec<slug>)`,
  unchanged.
- **Within a group** (both endpoints are extra cards of the same provider) → account
  reorder → new `SetAccountOrder(provider, Vec<account>)`.
- An expanded extra card **cannot be dragged across a group boundary** — the gesture is
  disallowed for that case so the two operations stay unambiguous. Collapsed groups (one
  visible card) drag as whole groups exactly as today.

This is the piece most likely to need a visual iteration once seen live.

## Tray

Already correct: `tray.rs::label` renders a provider shared by several accounts as
`Claude (work)`, and `needs_attention` ORs the 70%/90% threshold across every account. One
refinement: the label prefers `account_label` when present, falling back to the account id.

## Testing

- `Config::accounts` absent-key default, ordering, promotion rewrite, malformed-array
  refusal.
- `registry::accounts` builds one `Account` per configured account, in order, with the
  right ids.
- Secrets: per-account slot isolation (two accounts of one provider hold different keys).
- History: the existing per-account segment test stands; add the rename re-key migration
  (rows move to the new id, old id has none, transaction rolls back on a mid-failure).
- Engine: `add_account` persists + publishes pending; `remove_provider` on `"default"`
  promotes the survivor; `set_account_order` validates a permutation.
- Service: `AddAccount` / `SetAccountOrder` validation errors (duplicate id, unknown
  provider, malformed slug) surface as `InvalidArgs`.
- Tray: a shared provider renders `Name (label)`, preferring `account_label`.
- UI card grouping and the expand button are verified by driving the real window
  (browser/computer), not by a unit test.

## Out of scope

- CLI-source credentials for extra accounts.
- Multi-account for `None`/`External` providers.
- Aggregated/worst-case summary on a collapsed card.
- Persisting expand state across restarts.
