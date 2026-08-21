# Provider Management and Antigravity OAuth Design

- Status: approved
- Date: 2026-08-21
- Replaces: the provider-settings portion of implementation step 11

## Purpose

Step 11 must make providers explicitly configurable rather than drawing one settings group
for every provider compiled into Tidemark. A fresh installation starts with no providers.
The user adds only the services they want Tidemark to poll, edits each provider on its own
page, and can remove a provider without deleting its quota history.

The same step must give Antigravity a Tidemark-owned OAuth path. An `agy` installation is
useful as a fallback for an existing Antigravity session, but it must not be required to
track Antigravity quota.

All documentation, source code, code comments, tests, logs, and interface copy are written
in English.

## Goals

- Keep the settings interface usable when the built-in catalog grows beyond sixty
  providers.
- Distinguish the catalog of providers in this build from the providers the user has
  added.
- Start a fresh installation with an empty added-provider list.
- Add and remove providers while the daemon and GUI are running.
- Remove a provider's Tidemark-owned credentials and main-window card while preserving its
  history.
- Let Antigravity users sign in through Tidemark and fetch quota without `agy`.
- Continue using an existing `agy` session as a fallback when no Tidemark OAuth credential
  is stored.
- Fix the settings dialog so it can be closed and opened repeatedly.

## Non-goals

- Multiple accounts for one provider. Account identifiers remain in every interface so
  this can be added without a storage migration, but v1 still exposes one `default`
  account per added provider.
- Deleting history when a provider is removed.
- Installing, configuring, or signing out of `agy` on the user's behalf.
- Importing browser cookies or embedding a browser.
- Downloading provider implementations at runtime. The catalog contains the providers
  compiled into the current build.

## Chosen Architecture

The daemon owns both the built-in catalog and the ordered set of added providers. The GUI
learns both over D-Bus and does not carry a second hard-coded provider list.

This separates two concepts that the current static registry conflates:

- A **provider definition** says what this build supports: stable slug, display name,
  authentication kind, optional external-session fallback, help text, and declared
  settings.
- A **configured account** is a provider definition the user has added. Only configured
  accounts are instantiated, polled, published by `GetStatus`, and shown as cards.

Keeping the catalog in the daemon prevents GUI/daemon version skew and leaves a future CLI
able to perform the same add, edit, and remove operations.

## Configuration

The root of `config.toml` contains an ordered array:

```toml
providers = ["claude", "antigravity"]

[provider.zai]
region = "global"
```

A missing file or missing `providers` key means an empty list. It does not mean all built-in
providers. Adding a provider appends its slug. Removing it removes the slug and its
provider-specific table, if one exists. Existing comments, ordering, and unrelated or
unknown keys remain untouched through `toml_edit`.

Duplicate slugs are normalized to the first occurrence when read and are never written.
Unknown slugs remain in the file so an older build does not destroy configuration written
by a newer build, but they do not become accounts until the running build has a matching
catalog entry. The daemon logs them as unsupported.

Quota history remains keyed by provider and account exactly as it is now. Removing and
later re-adding a provider resumes the same historical series.

## D-Bus Contract

The wire layer gains a provider-definition dictionary and the daemon exposes these
operations:

- `ListProviders() -> Vec<ProviderDefinition>` returns the complete compiled-in catalog.
- `AddProvider(provider)` validates the slug, persists it, creates its default account,
  publishes the initial status, and schedules an immediate credential probe and poll.
- `RemoveProvider(provider, account)` performs the removal sequence described below.
- `ProviderRemoved(provider, account)` tells connected clients to remove the settings row
  and main-window card.

The existing `ProviderChanged(ProviderStatus)` signal continues to add or update a card.
`GetStatus()` continues to return only configured accounts. The GUI obtains the available
catalog once per daemon connection and computes the addable list by subtracting the current
statuses.

`ProviderDefinition` contains presentation metadata rather than provider behavior:

- `provider`: stable slug;
- `title`: user-facing name;
- `credential`: `key`, `oauth`, or `external`;
- `credential_hint`: concise help shown on the provider page;
- optional `external_fallback`: a user-facing name such as `agy session` or `Codex CLI
  login`;
- settings declarations with their choices and descriptions.

Provider-specific secrets and network logic remain in `tidemark-core` and `tidemarkd`.

## Dynamic Daemon State

Topology changes are serialized through the engine command queue. An add or remove command
carries a one-shot reply so the D-Bus method completes only after the engine has accepted
or rejected the operation.

Adding a provider performs these steps:

1. Validate that the slug exists in the catalog.
2. Treat an already-added provider as an idempotent success.
3. Build its account description and validate its configured settings.
4. Persist the slug.
5. Insert and announce the account.
6. Probe credentials and poll immediately.

Removing a provider performs these steps:

1. Cancel any pending OAuth login for the account.
2. Delete both possible Tidemark-owned Secret Service entries for that provider/account:
   API key and OAuth token. Deleting a missing entry is successful. Vendor files and `agy`
   sessions are never changed.
3. If Secret Service cannot confirm deletion, stop and leave the provider configured.
4. Remove the slug and its settings table from `config.toml`.
5. Remove the account from the engine, dropping its client and stopping future polls.
6. Remove its published status and emit `ProviderRemoved`.

If configuration persistence fails after credential deletion, the provider remains added
but becomes unauthenticated. The failure is returned to the UI and the next status makes
the missing credential visible. This is preferable to claiming successful deletion while
a secret remains stored.

## Settings Interface

### Main providers page

The `Providers` preferences dialog opens on a list containing only added providers. Its
header has an add button with the `+` icon.

Each row contains the provider mark, provider name, concise connection state, an edit
button with the pencil icon, and a destructive remove button with the trash icon. The edit
button pushes that provider's detail page. The row itself does not hide a second click
target behind the explicit buttons.

When no provider is added, the page shows:

- Title: `No providers added`
- Description: `Use + to add a provider.`

Removing a provider requires confirmation:

- Title: `Remove {provider}?`
- Body: `This removes the provider and its saved credentials. Quota history will be kept.`
- Actions: `Cancel` and `Remove`, with `Remove` styled as destructive.

### Provider picker

The add button pushes a dedicated provider-picker page. It contains a search entry labelled
`Search providers` and a scrollable list of catalog entries not already configured.
Filtering is case-insensitive and matches both display name and stable slug.

Selecting a provider adds it immediately and pushes its detail page. Closing or leaving
that page before credentials are supplied does not undo the add; the provider remains in
the list and its card honestly reports that credentials are missing.

### Provider detail page

The detail page has a large symbolic provider mark centered at the top and the provider
name directly below it. The existing installed icon convention is reused, and a missing
trademark asset remains a supported state rather than showing a broken-image icon.

Authentication and provider-specific settings follow in separate preference groups:

- OAuth providers show current state and `Sign in…` or `Sign out`.
- API-key providers show an empty masked `API key` field and `Save` or `Replace`. A stored
  key is never sent back over D-Bus or rendered.
- External-only providers explain where their session comes from and expose no secret
  control.
- Provider-declared settings, such as the Z.ai region, appear below authentication.

For an OAuth provider with an external fallback, the state copy distinguishes:

- `Signed in through Tidemark` when a Tidemark token is stored;
- `Using {external fallback}` after a successful poll without a stored Tidemark token;
- `Not signed in` when neither source is usable;
- `Checking for {external fallback}…` while the initial credential probe has not completed.

Navigating back keeps an OAuth attempt alive and shows its waiting state in the provider
list. Closing the entire preferences dialog cancels pending attempts and releases their
callback ports.

### Main-window empty state

With no configured providers, the existing main-window message page shows:

- Title: `Welcome to Tidemark`
- Description: `Add a provider to start tracking your quota.`

The providers button remains enabled while the daemon is connected, including in this
empty state.

## Antigravity OAuth and Direct Quota Fetching

Antigravity changes from external-only authentication to OAuth with an optional `agy
session` fallback.

The browser flow follows the working Antigravity desktop-client protocol established by
oh-my-pi and pi-antigravity:

- Google authorization endpoint: `https://accounts.google.com/o/oauth2/v2/auth`;
- token endpoint: `https://oauth2.googleapis.com/token`;
- registered callback: `http://localhost:51121/oauth-callback`;
- PKCE and validated `state` through the existing loopback login implementation;
- offline access and the Google Cloud, profile, email, cclog, and
  experiments/configuration scopes required by Cloud Code Assist;
- the public desktop-client secret is included where Google's token exchange requires it.

After the exchange, the daemon calls `loadCodeAssist` to discover the account's Cloud AI
Companion project. If the account has no project, it completes the onboarding operation
observed in both reference implementations and waits for provisioning with bounded
retries. The stored Secret Service
document contains access token, refresh token, expiry, and project id. Token refresh is
single-owner daemon work and preserves a rotated refresh token.

Direct quota polling uses the OAuth access token and project id with
`v1internal:fetchAvailableModels`. The adapter parses the direct API payload independently
from the existing local-RPC payload, groups shared counters without letting one healthy
model mask an exhausted sibling counter, converts remaining fractions to used percentages,
and preserves declared reset times and window identities.

Tidemark identifies itself as `Tidemark/<version>` in HTTP `User-Agent` headers. Required
Cloud Code Assist metadata is sent as protocol metadata; the implementation does not copy
randomized browser or Antigravity executable user agents from prior art.

Credential selection is:

1. Use and refresh a Tidemark-owned OAuth token when present.
2. Otherwise, try the existing `agy` supervisor and local quota RPC.
3. If `agy` is absent or has no signed-in session, publish `no-credential` rather than
   `unreachable`.

A rejected or non-refreshable stored token publishes `credential-rejected` and does not
silently fall back to `agy`; an explicit Tidemark login represents the user's chosen
account and must not be masked by another local session. Signing out removes the Tidemark
token and makes the next poll eligible to use `agy` again.

## Dialog Lifecycle Fix

The current implementation decides whether a closed `AdwPreferencesDialog` is open by
reading `GtkWidget.visible`. That property describes the widget's requested visibility,
not whether the adaptive dialog is still presented in its parent, so the retained Rust
object can be mistaken for an open dialog forever.

The main window instead owns an explicit dialog slot that is cleared from the
`AdwDialog::closed` signal. Clicking the providers button presents a new dialog when the
slot is empty and refuses duplicates only while a dialog is actually presented. Dialog
closure also cancels its outstanding login operations before releasing the final strong
reference.

## Error Handling

- Unknown catalog slugs and requests for unconfigured accounts return clear D-Bus argument
  errors.
- Config parse errors are never replaced with defaults. Mutations fail and leave the last
  valid in-memory topology running.
- Keyring, OAuth, project-discovery, and quota errors are returned as one-sentence toast
  messages and as the existing provider states where they affect polling.
- A failed add leaves no partial account or card.
- A failed remove remains visibly configured unless both credential deletion and config
  mutation have completed.
- A browser-launch failure leaves the login waiting and exposes the existing `Copy link`
  action.
- OAuth cancellation is idempotent, including dialog closure racing with the explicit
  `Cancel` button.
- Last good quota readings remain visible under transient failures.

## Test Strategy

Implementation follows test-driven development. Automated coverage includes:

- missing configuration produces an empty added-provider list;
- add order survives a write/read round trip and duplicate adds are idempotent;
- removal preserves comments, unrelated unknown settings, and history while removing the
  selected provider table;
- the catalog survives D-Bus serialization and contains complete metadata for every
  compiled provider;
- dynamic add publishes a pending account and schedules an immediate poll;
- dynamic remove stops polling, removes the published status, and emits
  `ProviderRemoved`;
- removal deletes API-key and OAuth-token slots for only the selected account;
- a keyring deletion failure leaves the provider configured;
- catalog search is case-insensitive and excludes configured providers;
- the GUI model adds and removes the corresponding card on topology signals;
- the dialog slot clears on `closed`, permits a second open, and never stacks two dialogs;
- closing the dialog cancels pending OAuth work;
- Antigravity authorization URL, callback, scopes, token exchange, refresh rotation,
  project discovery, and onboarding are exercised with deterministic HTTP fixtures;
- direct Antigravity quota payloads cover shared-counter deduplication, exhausted counters,
  missing optional fields, and malformed known entries;
- Antigravity prefers stored OAuth, falls back to a valid `agy` session only when no token
  is stored, and reports `no-credential` when neither is available.

Final verification runs the focused tests during each red-green cycle, then
`cargo test --workspace`, formatting, linting, and `scripts/check-layering.sh`. A GUI smoke
check opens, closes, and reopens the providers dialog; adds a provider; edits it; removes
it; and confirms its card disappears while historical rows remain in the database.
