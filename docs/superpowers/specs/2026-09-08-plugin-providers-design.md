# Plugin Providers — Design

> **Status:** Draft for user review, pre-implementation.
> **Scope:** User-installed providers that make one `GET` or empty `POST` request to a
> user-supplied metrics URL, put one API key in a header, parse one JSON response, and
> describe a Tidemark card.

## Why

Tidemark's provider catalog is compiled into the application. That is appropriate for
public providers which Tidemark owns and supports, but it prevents a company or a private
service from adding its metrics endpoint without publishing its protocol and branding in
the Tidemark repository.

A plugin provider is one human-readable file. Its author describes the provider, the
request shape, a pure JSON-to-metrics transformation, the card layout, and an SVG mark.
Another user imports the file, enters their own complete metrics URL and API key, and gets
the same polling, stale-state, history, pace and notification behavior as a built-in
provider.

The file is treated as untrusted data. Importing it must not grant filesystem, process,
environment, keyring, clock or network access. Tidemark owns the only network request and
never exposes the API key to plugin code.

## Scope boundary

Version 1 supports exactly:

- one complete endpoint URL entered by the account owner, not supplied by the plugin;
- one `GET` or one `POST` with an empty body;
- one API key inserted by Tidemark into a plugin-declared header name and optional prefix;
- one JSON response document;
- a pure, sandboxed Lua 5.4 transformation;
- percentages, numeric or textual values, `X of Y`, balances and quota windows;
- several gauges and values in a plugin-selected order;
- explicit placement on the card or in ordered detail sections;
- reset timestamps and window lengths for existing history, pace and notifications;
- at most one embedded, sanitized SVG provider mark;
- any number of configured accounts, each with its own endpoint URL and key.

Version 1 does not support:

- OAuth, browser cookies, vendor credential files or login flows;
- local service discovery, relaxed TLS, client certificates or Unix sockets;
- multiple requests, redirects, request bodies, JSONL or non-JSON responses;
- plugin-supplied origins or endpoint paths;
- arbitrary HTTP logic, retry policy or polling policy;
- filesystem, process, environment, keyring, clock or network APIs in Lua;
- arbitrary GTK widgets, CSS, Pango markup, HTML or executable SVG content;
- native libraries, WASM or external interpreter processes.

These are deliberate capability boundaries, not deferred parts of the first format. A
provider outside them remains a built-in provider unless a later format version explicitly
adds a safe, general capability.

## Invariants

- The account owner, not the plugin author, chooses the exact URL that receives the key.
- The key exists only in Tidemark's secret store and the host-owned request header. Lua
  never receives it.
- Missing provider values stay missing. The plugin runtime never substitutes zero, a reset
  timestamp, a maximum or a percentage.
- Metric identity is stable and separate from presentation. The same metric may be rendered
  as a gauge, a value or a ratio without changing history identity.
- Card and detail order are arrays in the plugin result and are preserved exactly.
- Plugins select among semantic Tidemark widgets. They cannot draw arbitrary UI or bypass
  the theme and accessibility model.
- Built-in and plugin providers use one GTK presentation path. A second plugin-only card
  renderer is not introduced.
- Fetch, JSON, Lua and output-validation failures preserve the account's last good reading
  through the existing `ProviderStatus` failure semantics.
- Plugin identifiers are persistent storage keys. Updating a definition never silently
  changes its identifier or existing account credentials.
- Linux and Windows execute the same file and expose the same Lua API.

## File format

The extension is `.tidemark-provider`. The contents are TOML because it is already used by
Tidemark, is readable without special tooling, has an unambiguous parser, and supports
literal multiline strings for Lua and SVG.

```toml
format_version = 1

[provider]
id = "com.acme.quota"
name = "Acme AI"
plugin_version = "1.0.0"

[request]
method = "GET"
api_key_header = "Authorization"
api_key_prefix = "Bearer "

[parser]
language = "lua54"
source = '''
function parse(response, context)
    local period = response.lastMonth
    local total_limit = period.limits.cost.amount
    local sonnet = nil

    for _, model in ipairs(period.models) do
        if model.modelId == "Sonnet 5" then
            sonnet = model
            break
        end
    end

    local metrics = {
        {
            id = "month-total-cost",
            title = "Monthly cost",
            value = period.cost.total,
            maximum = total_limit,
            remaining = period.limits.cost.remaining,
            used_percent = percent(period.cost.total, total_limit),
            unit = "USD",
        },
    }

    local card = {
        gauge("month-total-cost", { field = "used_percent" }),
    }
    local detail_items = {
        ratio("month-total-cost", { left = "value", right = "maximum" }),
    }

    if sonnet ~= nil and sonnet.limits.cost.amount ~= null then
        table.insert(metrics, {
            id = "month-sonnet-cost",
            title = sonnet.modelName,
            value = sonnet.cost.total,
            maximum = sonnet.limits.cost.amount,
            remaining = sonnet.limits.cost.remaining,
            used_percent = percent(sonnet.cost.total, sonnet.limits.cost.amount),
            unit = "USD",
        })
        table.insert(card, value("month-sonnet-cost", { field = "value" }))
        table.insert(
            detail_items,
            ratio("month-sonnet-cost", { left = "value", right = "maximum" })
        )
    end

    return {
        metrics = metrics,
        card = card,
        details = {
            {
                title = "Limits",
                items = detail_items,
            },
        },
    }
end
'''

[icon]
svg = '''
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <path fill="currentColor" d="..."/>
</svg>
'''
```

The plugin file contains no default endpoint. This keeps a shared corporate definition
independent of deployment topology and prevents the file's author from choosing where a
recipient sends a secret.

### Identity and compatibility

- `format_version` is the integer Tidemark file-format version. Unsupported versions are
  rejected before Lua compilation.
- `provider.id` is lowercase ASCII reverse-DNS style, matches
  `[a-z0-9]+(?:[.-][a-z0-9]+)*`, and contains at least one dot.
- Built-in provider IDs and the `tidemark.*` namespace are reserved.
- `provider.name` is plain text and bounded in length.
- `provider.plugin_version` is a SemVer string shown during import and replacement. It is
  informational for polling; `format_version` controls compatibility.
- Re-importing the same `provider.id` offers replacement. Existing accounts, endpoint URLs,
  keys, history and notification preferences remain attached to the stable ID.
- Removing an installed definition is refused while accounts using it remain configured.
  The user removes those accounts first, using the existing removal semantics.

### Request declaration

`request.method` accepts only `GET` and `POST`. A `POST` always has an empty body.
`api_key_header` is an RFC-valid header name. `api_key_prefix` is a bounded plain string;
common values are `"Bearer "` and `""`.

Tidemark rejects header names whose semantics can alter routing, proxying or connection
handling, including `Host`, `Cookie`, `Connection`, `Content-Length`, `Transfer-Encoding`,
`Proxy-Authorization`, `Proxy-Connection`, `TE`, `Trailer` and `Upgrade`. Tidemark supplies
its normal application identity and `Accept: application/json`; plugins cannot override
them or add a second header.

The account URL is absolute. HTTPS is accepted normally. Plain HTTP requires an explicit
per-account insecure-transport confirmation because its API key is visible on the network;
the plugin cannot request or remember that confirmation. URL credentials and fragments are
rejected. Redirect following is disabled so the secret header cannot move to another
origin. Existing application proxy policy and normal certificate validation apply.

## Lua transformation

### Why Lua

The runtime is vendored Lua 5.4 embedded through `mlua`; users install no interpreter.
Lua is widely used for human-authored configuration and supports the loops, filtering,
fallbacks, arithmetic and dynamic arrays present in real provider responses.

A release-size probe using Rust 2024, `serde_json`, Tidemark's thin-LTO/stripped release
profile and the same JSON arithmetic measured:

| Runtime | Increment over baseline |
|---|---:|
| `starlark 0.14.2` | 7,503,496 bytes (about 7.16 MiB) |
| `rhai 1.26.0` | 3,130,136 bytes (about 2.99 MiB) |
| `mlua 0.12.1` with vendored Lua 5.4 | 469,384 bytes (about 0.45 MiB) |

Starlark and Rhai are rejected for this feature because their measured cost is high relative
to Tidemark's binary. The implementation must measure the final stripped `tidemarkd`
release delta. If the Lua runtime and plugin machinery add more than 1.5 MiB on Linux, the
dependency decision returns to design review rather than silently accepting the increase.
The repository's Windows GNU target must compile and execute the same fixture in CI before
the feature is complete.

### Execution contract

A plugin must expose one entry-point function:

```lua
function parse(response, context)
    return presentation
end
```

The plugin must define `parse` and may define pure helper functions and constants in the
same source block. Top-level evaluation and the `parse` call run under the same sandbox
and resource limits.

`response` is the parsed JSON document. JSON objects become Lua tables with string keys;
arrays become one-based sequences; strings, booleans and finite numbers retain their JSON
meaning. JSON `null` becomes the read-only sentinel `null`, so it remains distinguishable
from a missing table key.

`context` contains only:

```lua
{
    captured_at = 1788870896
}
```

`captured_at` is Unix seconds. There is no `now()` function. `parse_time` converts a reset
timestamp from the response to the same representation, so deterministic timestamp
arithmetic remains possible. Re-running the same response and context produces the same
result.

The available standard-library subset is `assert`, `error`, `ipairs`, `select`, `tostring`,
`type`, `math`, `string` and `table`, after removing functions which load/dump executable
chunks and `math.random`/`math.randomseed`. `dofile`, `load`, `loadfile`, `require`,
`collectgarbage`, `next`, `pairs`, `pcall`, metatable mutation, coroutines, `debug`, `io`,
`os`, `package` and native module loading are absent. Removing unordered object iteration
keeps output stable across platforms; authors use `sorted_keys` when a JSON object's keys
must be traversed. The environment is created by Tidemark for every execution and is not
shared between accounts or polls.

Tidemark adds a small documented pure API:

- `number(value)` — finite number or a numeric string; otherwise a parse error;
- `percent(value, maximum)` — `value / maximum * 100`, rejecting a non-positive maximum;
- `parse_time(value)` — RFC 3339 string or Unix seconds to Unix seconds;
- `is_null(value)` — true only for the JSON-null sentinel;
- `sorted_keys(object)` — a lexically sorted array of an object's string keys;
- `gauge(metric_id, options)` — semantic quota bar reference;
- `value(metric_id, options)` — semantic standalone value reference;
- `ratio(metric_id, options)` — semantic `X of Y` reference;
- `status(metric_id, options)` — semantic plain status text reference.

These helpers return typed marker tables; they do not access UI, state or I/O. Formatting
uses Tidemark's shared presentation functions after validation, not arbitrary Lua format
strings, so GUI and CLI values remain consistent.

### Resource limits

The daemon enforces limits independently of the script:

- plugin file: 512 KiB including Lua and SVG;
- Lua source: 128 KiB;
- HTTP response body: 4 MiB before JSON parsing;
- Lua heap attributable to one execution: 16 MiB;
- Lua VM instructions: 1,000,000 per execution;
- output metrics: 128;
- card items: 32;
- detail sections: 32, each with at most 128 items;
- metric IDs, labels, values and error excerpts have bounded lengths.

The exact constants live together in the plugin runtime and are documented verbatim. Limit
exhaustion is a provider failure, not a daemon crash or a partially accepted presentation.

## Semantic output model

The parser returns three fields: `metrics`, `card` and `details`.

### Metrics

`metrics` is an array. Every metric has:

- `id`: stable plugin-local identity;
- `title`: plain display label;
- optional `subtitle`;
- any applicable numeric fields: `value`, `maximum`, `remaining`, `used_percent`;
- optional plain `text` for a non-numeric status;
- optional `unit`, selected from documented units or a bounded plain suffix;
- optional `window` metadata.

A metric's numeric fields are independent of how it is drawn. For example, one cost metric
can be used by all of these without being parsed again:

```lua
{
    gauge("month-total-cost", { field = "used_percent" }),
    value("month-total-cost", { field = "remaining" }),
    ratio("month-total-cost", { left = "value", right = "maximum" }),
}
```

`window` contains a stable `key`, optional `resets_at`, and optional positive
`length_seconds`. A metric with `window` and `used_percent` becomes a normal Tidemark
`Window`, participating in history, pace and per-window notifications. A metric without
window metadata remains presentational and is not fabricated into history.

### Widgets

`card` is an ordered array of semantic widget references. `details` is an ordered array of
sections, each containing an ordered `items` array. The same metric may be referenced more
than once. A plugin author chooses the layout in the file; import does not expose a second
GUI layout editor or a preset system.

- `gauge` displays a bar and the selected numeric field. It requires either a finite
  `used_percent` or finite numerator and positive denominator fields.
- `value` displays one numeric or text field using a semantic number, percent, currency,
  duration or plain-text format.
- `ratio` displays two finite numeric fields as `X of Y`.
- `status` displays bounded plain text with one of Tidemark's semantic states; it cannot
  supply colors or markup.

Widget options may choose fields, shared formatting and compact/normal emphasis. They
cannot specify pixels, fonts, colors, CSS classes, markup or GTK properties. This preserves
the theme, fixed card geometry, accessibility and future UI changes.

### Typed validation

The Lua return value is converted into Rust types before publication. Validation rejects
the whole reading when:

- metric IDs are duplicated or malformed;
- a widget references an absent metric;
- a referenced field is missing or has the wrong type;
- a gauge denominator is absent, non-finite or non-positive;
- a ratio operand is absent or non-finite;
- `used_percent`, timestamps or window lengths are malformed;
- a detail section references an item outside the supported semantic types;
- strings, collections or nesting exceed limits;
- any Lua value is a function, userdata, thread or cyclic table.

Values greater than 100 percent remain truthful data. The renderer may visually cap a bar
at its endpoint while displaying the actual number. Unknown and null remain absent; a
script includes a widget conditionally when the source does not always provide its fields.

## Shared presentation architecture

`Snapshot` remains the provider/core model consumed by history and notification policy.
A new shared semantic presentation shape is added to `tidemark-types` and carried in the
extensible `ProviderStatus` dictionary. The daemon produces it for every account:

- plugin providers use the validated Lua result;
- built-in providers use one Rust adapter which maps their existing ordered windows,
  `Plan` and `Balance` conventions to the same semantic widgets.

The GTK card renders only the shared semantic presentation. Existing built-in appearance
is reproduced by the adapter before the plugin path is enabled. This is a clean cutover,
not a permanent legacy renderer beside a plugin renderer.

The wire shape is semantic rather than GTK-specific. `tidemarkctl` may ignore layout when
printing existing usage formats, while future consumers can render the same metric and
order without importing GTK or plugin code. Absent presentation in an older daemon remains
a compatibility case during rolling upgrades; the GUI's compatibility fallback is removed
once the minimum daemon version includes the new shape.

## Ownership by crate

- `tidemark-types`: plugin-independent metric and semantic presentation wire vocabulary.
  It performs no parsing, I/O or policy.
- `tidemark-core`: `.tidemark-provider` schema, SVG sanitizer, Lua sandbox, typed output
  validation and generic single-request `Provider` implementation.
- `tidemarkd`: installed-definition storage, account endpoint configuration, keyring access,
  dynamic registry entries, import/update/remove operations and D-Bus publication.
- `tidemark-ipc`: the single generated proxy contract for plugin management and extended
  provider definitions/status.
- `tidemark`: file chooser, import preview, plugin/account settings and the generic semantic
  GTK renderer. It performs no HTTP, Lua execution or plugin policy.
- `tidemark-cli`: `plugin validate` and `plugin render --response FILE` argument handling,
  local file reads and diagnostic formatting. It sends plugin/fixture bytes to daemon
  D-Bus methods and does not depend on core.

The last point preserves the enforced rule that `tidemark-cli` does not depend on core.
There is one plugin parser and one runtime, both owned by core and reached through the
daemon.

## Per-account endpoint configuration

The existing generic provider-option path cannot store plugin endpoints. Today
`Config::option(provider, name)` and `Config::set_option` read and write values directly
under `[provider.<id>]`; `Engine::set_option` accepts an account only to select the client
to rebuild. Reusing that path would silently share one endpoint across all accounts.

Plugin endpoints therefore get an account-addressed decorated-TOML path:

```toml
[provider.\"com.acme.quota\"]
accounts = [\"default\", \"work\"]

[provider.\"com.acme.quota\".account.default]
endpoint = \"https://metrics.example.test/v1/usage\"
allow_insecure_http = false

[provider.\"com.acme.quota\".account.work]
endpoint = \"https://metrics.corp.test/v1/usage\"
allow_insecure_http = false
```

`Config` gains account-addressed plugin-endpoint read/write/remove operations which
preserve TOML decoration and reject a present-but-invalid value. The daemon exposes the
same addressability as `SetPluginEndpoint(provider, account, endpoint,
allow_insecure_http)`. It is not implemented as a hidden `ProviderOption`.

Removing an account removes its endpoint and insecure-transport acknowledgement. Removing
or replacing a plugin definition does not migrate endpoint values to another provider ID.
Key storage remains unchanged and correctly uses `(provider, account)`.

## Import and account flow

1. The GUI or `tidemarkctl plugin validate FILE` sends file bytes to the daemon.
2. The daemon parses TOML, validates metadata/request/SVG, compiles Lua and returns an
   inspection result without writing the file.
3. The inspection UI shows ID, name, plugin version, request method, secret-header name and
   prefix, declared format limits, SVG preview and diagnostics.
4. On explicit installation, the daemon repeats validation and atomically writes the exact
   validated bytes under the user data directory, named from the stable provider ID.
5. The definition appears in the provider catalog.
6. Adding an account asks for account name, complete endpoint URL and API key. The final URL,
   method and secret-header shape are shown together before the first request.
7. The endpoint is stored as per-account non-secret configuration. The key is stored under
   the existing `(provider, account)` keyring identity.
8. The registry constructs a generic plugin account. Existing engine scheduling, refresh,
   publication and last-good state apply unchanged.
9. A successful response is parsed and validated into `Snapshot` plus semantic
   presentation, then flows through history, notifications, D-Bus, tray, CLI and GUI.

Importing a definition does not configure an account and performs no network request.
`plugin render FILE --response RESPONSE.json` executes the pure parser against a local
fixture through the daemon and returns the semantic result without reading or storing an
API key. This is the primary authoring/debugging loop documented for plugin writers.

## SVG handling

The SVG is data inside the TOML file. Import parses XML and accepts a conservative static
subset sufficient for Tidemark provider marks: `svg`, grouping, paths and basic geometric
shapes, transforms, solid fills and `currentColor`. It rejects scripts, event attributes,
animation, foreign objects, filters, external stylesheets, external or data URLs, embedded
raster images and references outside the same document.

The sanitizer serializes the accepted tree to canonical SVG bytes. The original validated
TOML document is stored unchanged so it remains inspectable and exportable; only canonical
SVG bytes enter the in-memory provider definition and D-Bus metadata. The GUI renders the
sanitized bytes through its normal image stack and never resolves a plugin-controlled
external resource. Missing SVG is allowed and uses the existing generic provider mark.

## Errors and diagnostics

Errors are contextual and identify one stage: file schema, request declaration, SVG, Lua
compile, HTTP status, response size, JSON parse, Lua runtime, resource limit or semantic
output. Lua diagnostics include plugin-relative line and column plus a bounded message;
they never include the API key, request headers or a whole response body.

Import/update is transactional. A rejected replacement leaves the installed definition
untouched. A poll failure publishes the existing unavailable/rate-limited/error state while
retaining last-good windows and presentation. It never publishes a partially validated
metric set.

The debug response recorder applies the existing secret rules and additionally replaces an
exact occurrence of the account key in a response body before persistence, covering a
misbehaving endpoint that echoes its credential.

## Real-response feasibility check

The live `~/.local/share/tidemark/debug/responses.ndjson` corpus was inspected without
copying it into the repository. It contained successful JSON responses for Z.ai,
Antigravity, NanoGPT, OpenRouter, Codex, Claude, Kimi, DeepSeek and OpenCode Go; T3 Chat used
JSONL. Groq had only `401` responses with empty bodies, and Wayfinder had only a transport
error, so their parsing could not be validated from successful real data.

The successful shapes require nested objects, arrays, numeric strings, nullable fields,
arithmetic, `used = limit - remaining` fallbacks, filtering, dynamic iteration, formatting
and timestamp parsing. The specified Lua subset expresses all of those operations. The
proxy example in this document additionally proves that a single response can expose a
total cost, per-model costs, tokens and several periods, then select any combination of
gauge, value and ratio widgets.

Transport parity is intentionally narrower:

| Provider/response | JSON-to-card transform | Existing full transport fits v1 |
|---|---:|---:|
| User's proxy example | yes | yes |
| Kimi | yes | yes |
| DeepSeek | yes | yes |
| OpenCode Go | yes | yes |
| Z.ai | yes | no; current card joins two requests |
| NanoGPT | yes | no; current card uses parallel GET and POST |
| OpenRouter | yes | no; current card joins required and optional requests |
| Claude | yes | no; OAuth/vendor credentials |
| Codex | yes | no; OAuth refresh/vendor credentials |
| Antigravity | yes | no; local RPC/session discovery |
| T3 Chat | yes as a pure transform | no; JSONL, browser cookies and special transport |
| Groq | unconfirmed from corpus | no; current implementation makes four requests |
| Wayfinder | unconfirmed from corpus | no; current implementation makes three requests |

This distinction is important: Lua is not the limiting factor for the observed successful
JSON bodies. The one-request API-key capability boundary is.

## Documentation and authoring experience

The feature ships with one maintained plugin-author guide, linked from README:

1. a complete five-minute example using a single nested JSON response;
2. the TOML field reference;
3. the exact JSON-to-Lua mapping and null behavior;
4. every available Lua global and the list of unavailable libraries;
5. the metric schema and a visual gallery of `gauge`, `value`, `ratio` and `status`;
6. conditional fields, loops, model lookup, numeric strings and fallback recipes;
7. embedded SVG rules and a sanitizer error reference;
8. `tidemarkctl plugin validate` and `plugin render` authoring workflows;
9. security and resource limits;
10. versioning, replacement and compatibility rules.

A repository example plugin and sanitized response fixture exercise the same code shown in
the guide. Documentation never instructs authors to place a real endpoint, API key or live
recorded response in a plugin or fixture.

## Verification

### Pure behavior

- Parse a complete plugin and reject every malformed required field.
- Transform the sanitized proxy fixture into total and per-model metrics.
- Render the same metric as gauge, standalone value and ratio without changing its ID.
- Preserve card and detail order exactly.
- Handle numeric strings, JSON null, missing optional values, dynamic model lookup,
  arithmetic fallback and RFC 3339/Unix timestamps.
- Reject dangling widget references, duplicate metric IDs, invalid denominators, non-finite
  numbers, cyclic Lua tables and unsupported output values.
- Convert window metrics into the existing history/pace/notification model without
  fabricating missing reset metadata.

### Sandbox and input abuse

- Stop infinite loops and recursion at resource limits.
- Reject excessive allocation, output counts, nesting and string lengths.
- Prove that `io`, `os`, `package`, `debug`, `require`, native loading and bytecode loading
  are unavailable.
- Reject SVG scripts, events, external references, embedded raster data and oversized
  documents.
- Keep keys and response bodies out of diagnostics.

### HTTP integration

A local test server verifies both `GET` and empty `POST`, the exact configured key header,
normal proxy/TLS policy, disabled redirects, response-size enforcement, non-2xx handling,
malformed JSON and last-good preservation. Tests assert that the Lua input and published
wire form contain no credential.

### Cross-platform and size

- Linux and `x86_64-pc-windows-gnu` CI compile the vendored Lua configuration.
- Both targets execute the same parser fixture and produce the same typed result.
- The final stripped Linux `tidemarkd` release delta is recorded against the pre-feature
  commit and must stay at or below 1.5 MiB unless this design is explicitly revised.

### Actual surface

Using a fake endpoint rather than the test file alone:

1. import the example `.tidemark-provider`;
2. add an account and store a test key;
3. fetch one response through `GET` and one through empty `POST`;
4. observe the configured gauge/value/ratio order on the GTK card;
5. open details and verify its independent order;
6. verify the sanitized embedded SVG;
7. restart daemon and GUI and verify definition, endpoint, account and key persistence;
8. trigger malformed JSON and verify last-good values remain visible with failure state;
9. remove the account, then uninstall the definition.

## Acceptance criteria

A person can author and understand one `.tidemark-provider` file using the published guide,
validate it without a live key, share it, import it on Linux or Windows, enter their own
complete endpoint and key, and receive a card whose metrics, widget types, order, details,
SVG, history, pace and notifications match the file. The plugin cannot choose a destination
for the key or access anything beyond its JSON input and deterministic context. The final
binary-size measurement remains inside the stated budget.
