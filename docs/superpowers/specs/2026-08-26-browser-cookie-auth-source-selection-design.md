# Browser-Cookie Authentication Source Selection Design

## Goal

Let a person choose the exact local authentication source a browser-cookie provider uses.
Cursor is the first consumer: Cursor App or one browser, optionally a profile in that browser.
The mechanism is generic so a future browser-cookie provider does not add provider-specific GUI code.

## Scope

- Add a daemon-owned, data-driven authentication-source capability for external providers.
- Discover Cursor App and every supported browser store, then validate candidates with Cursor.
- Add a Cursor detail page using the existing authentication-source toggle, with Cursor App and Browser tabs.
- Persist an explicit source selection and never silently switch to another account.
- Keep cookies, tokens, browser databases, and HTTP access out of the GTK process and D-Bus output.

Out of scope: multiple quota cards per provider, manually entered cookies, writes to browser data,
automatic replacement of a failed source, and changes to OAuth/API-key flows.

## Constraints

- tidemark remains display-only and speaks only D-Bus; it does not depend on tidemark-core,
  reqwest, rusqlite, or browser storage.
- tidemarkd owns source discovery, remote validation, config mutation, and provider rebuild.
- Browser databases remain read through owner-only temporary snapshots; no browser directory is
  opened or modified in place.
- A cookie or Cursor App token is a Credential: never include it in wire data, config, logs,
  debug output, errors, or toasts.
- New wire values are optional and extensible; absent data remains absent.
- Browser slugs and profile directory identifiers are durable selected-source identities.
- A source selection is exclusive. Its failure reports a normal credential failure; it never
  falls back to another browser or Cursor App.

## Architecture

### Generic browser-auth model

tidemark-types gains secret-free structures for a provider's selectable external authentication:

- an authentication-selector definition naming its config option and top-level tabs;
- an opaque candidate identifier, title, optional subtitle, optional children, and verification state;
- selected mode and candidate metadata;
- states ready, missing, rejected/expired, waiting-for-keyring, and temporarily unreachable.

The selector definition travels with ProviderDefinition. Live candidates are fetched separately,
because availability is machine- and time-dependent. Browser/profile identifiers are opaque to the
GUI and expose neither cookies nor browser database paths.

tidemark-core::browser continues to own browser inventory and snapshot reads. A generic
browser-auth helper turns stores matching a provider-supplied cookie query into candidates,
filters expired cookies, and calls a provider-supplied asynchronous validator. A provider supplies
only its cookie query and its proof request. Future browser-cookie providers therefore reuse
discovery, source selection, state handling, and redaction without GUI branches by slug.

Cursor declares two source modes:

- cursor-app, from the Cursor standalone state database in ~/.config/Cursor;
- browser, from one selected supported browser and optionally one selected profile.

Cursor validation uses the same honest Tidemark HTTP client and request construction as polling.
A successful usage summary proves that a session works; an explicit credential rejection proves
that it does not. Validation publishes or retains no response body beyond the request scope.

### Daemon contract and lifecycle

The daemon adds two generic D-Bus operations:

1. Inspect sources for one configured account. It discovers and validates candidates in the daemon
   and returns only metadata and states.
2. Select one source. It revalidates the source, atomically writes the selection, removes stale
   subordinate fields, rebuilds the provider, publishes status, and schedules an immediate poll.

SetOption remains for static settings. It cannot set a browser source identifier because the
candidate must be revalidated immediately before the config write.

Cursor has provider-local stable settings:

- auth-source = cursor-app or browser
- auth-browser = browser slug, present only for browser mode
- auth-profile = profile identifier, present only for browser mode

Choosing Cursor App removes auth-browser and auth-profile. Choosing a browser resolves it to that
browser's first validated profile in stable scan order at selection time and records the resolved
profile alongside the slug, so every later poll reads exactly the proven store instead of
rescanning and quietly choosing a different account. A nested choice works the same way, naming
its explicit profile. Invalid/missing hand-edited values do not choose a different source; they
lead to the ordinary no-credential state.

Cursor receives this resolved selection through its options and reads only the selected store.
The historical implicit scan of every browser plus Cursor App is removed.

### GTK/libadwaita interface

Adding Cursor creates its account and immediately opens its existing ProviderDetail page.
The configured-provider list retains its existing document-edit-symbolic pencil button; Cursor is
eligible because it has authentication-source configuration.

The Authentication group uses the existing adw::ToggleGroup tab/pill interaction used by the
Claude, Codex, and Antigravity OAuth/CLI selector:

- Cursor App shows availability, a short state-database explanation, and Check again.
- Browser shows browser candidates and Check again. A browser with one usable profile is directly
  selectable. A browser with several usable profiles reveals only those profiles as nested choices.
  This rare case does not complicate the common one-click path.

Changing tabs switches the visible Authentication half immediately. Selecting a candidate becomes
authoritative only after the daemon accepts it; refusal restores the prior choice and shows a toast.

On opening the detail page, source content begins in a neutral checking state. The GUI calls
Inspect sources through glib::spawn_future_local. It does not read databases or make HTTP requests.
Candidate widgets are data-driven and do not match Cursor or browser slugs.

Ready candidates show a green check and are selectable. Missing, expired, and rejected candidates
show red status and are insensitive. Keyring-locked and transport failures are neutral rather than
red because they do not prove an account invalid; each permits Check again. Status wording always
accompanies colour. Existing detail-page open-close-open lifecycle behavior is preserved.

## Error handling and privacy

- An unreadable browser database makes that candidate unavailable without hiding other candidates.
- A locked Secret Service is the existing waiting state, never equivalent to not found.
- Network failure, timeout, and rate limit are inconclusive. They do not turn a candidate red or
  overwrite a previous selection.
- A 401/403 or expired session is red and insensitive. The selected source stays recorded so
  the provider reports the true failure instead of silently reading a different account.
- Source selection revalidates under the account mutation lock to close the inspection-to-write gap.
- Errors use existing query/credential redaction. D-Bus exposes descriptive states, never raw
  provider errors with paths or request details.

## Tests and acceptance

Core tests use temporary homes, copied SQLite fixtures, fake keyring storage, and loopback HTTP
servers. Cover Chromium and Gecko, missing/expired/rejected candidates, Cursor App, redaction,
stable browser/profile identifiers, browser-only and profile selection, and no cross-source fallback.

Daemon/type tests cover wire round trips, omitted optional capability, inspection of unknown accounts,
validation before config mutation, stale-field removal, rebuild, immediate poll, and D-Bus behavior.

GTK/model tests cover the two tabs, loading/retry/error states, selection rollback, green/red
sensitivity, nested profiles only when needed, add-to-detail navigation, pencil visibility, and
dialog reopen after source/status updates.

Final verification runs cargo fmt --all --check; cargo clippy --workspace --all-targets -- -D warnings;
dbus-run-session -- cargo test --workspace; scripts/check-layering.sh;
scripts/check-desktop-integration.sh; scripts/test-restart-user-daemon.sh; and shellcheck over the
repository's scripts and packaging hooks.

Installed-package acceptance verifies the new D-Bus contract and GTK provider page against the
installed daemon. It exercises Cursor App, a valid browser, and an unavailable browser without
moving the user's cursor.

## Documentation changes

CONTEXT.md replaces its deferred blanket exclusion of browser-cookie scraping with this bounded
mechanism and its privacy/ownership rules. README.md explains that Cursor offers an explicit local
source selector instead of silently using the first session it finds.
