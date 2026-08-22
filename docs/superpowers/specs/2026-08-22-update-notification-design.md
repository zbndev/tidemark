# Update availability notification design

## Goal

Tidemark does not install updates. It periodically discovers the latest published GitHub
release and, when that release is newer than the running daemon, exposes a single unobtrusive
link to the releases page in the main window. A failed check must not change anything the user
can see.

The daemon version is authoritative. The GUI binary's version is never used for the decision.

## Release contract

- Releases come from `zbndev/tidemark` through GitHub's public `releases/latest` REST endpoint.
- Only published full releases participate. GitHub's endpoint already excludes drafts and
  prereleases.
- Release tags have exactly the form `vX.X.X`, where each component is a semantic-version
  integer in canonical decimal notation (zero itself is allowed; leading zeroes are not).
  Tidemark does not support prerelease suffixes, build metadata, or arbitrary tag names.
- The tag without its leading `v` is compared as a semantic version with
  `tidemarkd`'s `CARGO_PKG_VERSION`.

An invalid response, invalid tag, invalid daemon version, non-success response, timeout, JSON
error, rate limit, or any other network failure is a failed check.

## Ownership and components

### Daemon release checker

A focused module in `tidemarkd` owns the GitHub request, response decoding, strict tag parsing,
and version comparison. It uses a small dedicated HTTP client with Tidemark's user agent, GitHub's
recommended media type and API-version headers, and a finite timeout.

The daemon starts one background checker task after its D-Bus object is available. The task waits
one minute before the first request and then checks once per hour. The initial delay keeps release
discovery outside the daemon's startup path and prevents rapid development restarts from issuing an
immediate burst of requests. No timestamp or response is persisted: the running daemon is the sole
owner of this transient information.

The checker holds an `Option<String>` containing the newer version. Each successful check derives a
new value:

- `Some(version)` when the latest release is newer than the running daemon;
- `None` when the latest release is equal to or older than the running daemon.

A failed check leaves the previous value unchanged. Failures may be written to the daemon log but
never become provider state, notifications, D-Bus errors delivered asynchronously to the GUI, or
visible messages. When a successful check changes the value, the daemon updates shared state before
emitting its D-Bus signal. The task is aborted during orderly daemon shutdown.

### D-Bus contract

The existing daemon interface gains:

- `GetUpdate() -> String`, returning the newer `X.X.X` version or an empty string when none is
  known;
- `UpdateChanged(String version)`, carrying the same representation whenever a successful check
  changes the value.

The empty string keeps the interface simple for `busctl` and permits a later successful check to
withdraw stale availability. As with provider publication, state is written before the signal, so a
client reacting to the signal cannot read an older value.

The GUI subscribes to `UpdateChanged` before its initial load. `GetUpdate` is auxiliary: if an older
daemon does not implement it or this one call fails, quota status still loads normally and the update
button remains hidden. Losing and regaining the daemon bus owner causes the GUI to reload update
availability along with the provider catalog and statuses.

### GUI

The main window creates a `software-update-available-symbolic` button in the header bar, packed
immediately to the left of the existing quota-refresh button. It starts hidden. A non-empty update
version shows it and sets a tooltip that names the available version; an empty value hides it.

Clicking the button opens `https://github.com/zbndev/tidemark/releases` with `gtk::UriLauncher`, using
the main window as the parent. Tidemark never downloads a package or chooses between DEB and RPM.
Failure to launch the browser is logged and leaves the button available for another attempt.

Disconnecting from the daemon hides the button because availability belonged to that daemon
instance. Reconnection restores it from `GetUpdate` if the newly connected daemon has already found
a newer release.

## Error handling and security

- The request is unauthenticated and sends no GitHub token, account identity, provider credential,
  machine identifier, or operating-system details.
- Only the fixed HTTPS API endpoint is requested; response URLs are not followed for application
  navigation.
- Only the fixed GitHub releases page is opened by the GUI. The response cannot supply a browser URL.
- The checker reads the response through an explicit small byte limit before decoding only the
  release tag, so a response cannot cause an unbounded allocation.
- Automatic-check failures remain silent in the interface. Logs contain error categories and status
  codes, never response bodies.
- There is no retry loop inside one interval. A failure waits until the next hourly check.

## Testing

Implementation follows test-driven development.

- Pure unit tests pin strict `vX.X.X` parsing and semantic ordering, including multi-digit
  components, equality, older releases, malformed tags, prerelease suffixes, and build metadata.
- Checker tests use a local HTTP fixture to cover success, non-success status, malformed JSON,
  malformed tags, and preservation of the previous state on failure. Production constants remain
  fixed; test construction injects the endpoint and timing where needed.
- D-Bus tests cover `GetUpdate`, state-before-signal ordering, a transition to an available version,
  and withdrawal through an empty `UpdateChanged` payload.
- GUI-side tests cover the empty/non-empty visibility decision and bus-event mapping. The mock daemon
  publishes a newer version so the real GTK window can be inspected with the button present.
- Final verification includes formatting, clippy with warnings denied, the full D-Bus workspace test
  suite, layering checks, installed-package verification, live installed daemon introspection, and a
  live GUI check that the update control appears in the required header position and opens the fixed
  releases page.

## Non-goals

- Downloading or installing DEB/RPM packages.
- Package-manager integration or detecting which package format installed Tidemark.
- Desktop notifications, badges on the tray icon, dismissing an available release, or manual
  "check for application updates" controls.
- Configurable channels, prereleases, arbitrary version names, or a user-configurable interval.
- Persisting release-check state across daemon restarts.
