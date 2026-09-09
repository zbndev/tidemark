# Writing a Tidemark provider plugin

A plugin teaches Tidemark to read one more quota endpoint. It is a single file — TOML
metadata, a pure Lua transformation and an optional mark — that a user imports and points at
a URL of their own choosing.

Two rules run through everything below, and the format is built so that neither can be
broken by accident:

- **A plugin never names a host.** The account owner types the whole URL. That is why a
  shared corporate definition is safe to circulate: the file cannot decide where somebody
  else's API key is sent.
- **A plugin never sees the key.** It is not in the response, not in the context table, not
  in a diagnostic. The parser is a pure function from a JSON document to a reading.

Everything here is checked against the code that enforces it. Where a number appears, it is
the number in
[`crates/tidemark-core/src/plugin/limits.rs`](../crates/tidemark-core/src/plugin/limits.rs).

**Contents**

1. [A plugin in five minutes](#1-a-plugin-in-five-minutes)
2. [The file format](#2-the-file-format)
3. [The response: JSON as Lua sees it](#3-the-response-json-as-lua-sees-it)
4. [The sandbox: what you get, and what is gone](#4-the-sandbox-what-you-get-and-what-is-gone)
5. [The reading: metrics, windows and layout](#5-the-reading-metrics-windows-and-layout)
6. [Recipes](#6-recipes)
7. [The provider mark](#7-the-provider-mark)
8. [The authoring loop](#8-the-authoring-loop)
9. [Security model and limits](#9-security-model-and-limits)
10. [Versioning, replacement and removal](#10-versioning-replacement-and-removal)

A [checklist](#checklist) closes the document.

---

## 1. A plugin in five minutes

Say the endpoint answers this — the file is
[`examples/plugins/acme-response.json`](../examples/plugins/acme-response.json):

```json
{
  "lastMonth": {
    "cost": { "total": "12.5", "currency": "USD" },
    "limits": { "cost": { "amount": 50, "remaining": 37.5 } },
    "requests": { "limit": 5000, "remaining": 1240 },
    "resetsAt": "2026-10-01T00:00:00Z",
    "models": [
      {
        "modelId": "sonnet-5",
        "modelName": "Sonnet 5",
        "requests": { "used": 320 },
        "limits": { "requests": { "amount": 1000, "remaining": 680 } }
      },
      {
        "modelId": "haiku-4-5",
        "modelName": "Haiku 4.5",
        "requests": { "used": 74 },
        "limits": { "requests": { "amount": null, "remaining": null } }
      }
    ]
  }
}
```

Then this is a complete plugin —
[`examples/plugins/acme-quota.tidemark-provider`](../examples/plugins/acme-quota.tidemark-provider):

```toml
format_version = 1

[provider]
id = "com.acme.quota"
name = "Acme AI"
plugin_version = "1.0.0"

[request]
method = "GET"
api_key_header = "X-Acme-Key"
api_key_prefix = ""

[parser]
language = "lua54"
source = '''
function parse(response, context)
    local period = response.lastMonth

    local cost_limit = number(period.limits.cost.amount)
    local cost_used = number(period.cost.total)

    local request_limit = number(period.requests.limit)
    local requests_used = request_limit - number(period.requests.remaining)

    local metrics = {
        {
            id = "month-cost",
            title = "Monthly cost",
            value = cost_used,
            maximum = cost_limit,
            remaining = number(period.limits.cost.remaining),
            used_percent = percent(cost_used, cost_limit),
            unit = "USD",
            window = {
                key = "month",
                resets_at = parse_time(period.resetsAt),
                length_seconds = 2592000,
            },
        },
        {
            id = "month-requests",
            title = "Requests",
            value = requests_used,
            maximum = request_limit,
            remaining = number(period.requests.remaining),
            used_percent = percent(requests_used, request_limit),
        },
    }

    local card = {
        gauge("month-cost", { field = "used_percent" }),
        value("month-requests", { field = "value" }),
    }
    local items = {
        ratio("month-cost", { left = "value", right = "maximum" }),
    }

    for _, model in ipairs(period.models) do
        local cap = model.limits.requests.amount
        if not is_null(cap) then
            local id = "model-" .. model.modelId
            local used = number(model.requests.used)
            table.insert(metrics, {
                id = id,
                title = model.modelName,
                value = used,
                maximum = number(cap),
                remaining = number(model.limits.requests.remaining),
                used_percent = percent(used, number(cap)),
            })
            table.insert(items, ratio(id, { left = "value", right = "maximum" }))
        end
    end

    return {
        metrics = metrics,
        card = card,
        details = { { title = "Limits", items = items } },
    }
end
'''

[icon]
svg = '''
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <path fill="currentColor" d="M32 6 6 58h12l14-28 14 28h12L32 6Z"/>
</svg>
'''
```

Check it and watch it run, without installing anything and without a key:

```bash
tidemarkctl plugin validate examples/plugins/acme-quota.tidemark-provider
tidemarkctl plugin render  examples/plugins/acme-quota.tidemark-provider \
  --response examples/plugins/acme-response.json
```

Then install it, configure an account, give it a URL and a key:

```bash
tidemarkctl plugin install examples/plugins/acme-quota.tidemark-provider
tidemarkctl provider add com.acme.quota
tidemarkctl plugin endpoint com.acme.quota https://metrics.example.test/v1/usage
tidemarkctl auth set-key com.acme.quota      # the key is read from stdin
tidemarkctl refresh com.acme.quota
```

In the GUI the same thing is one flow: **Providers → Import a provider file**, then the form
that asks for the URL and the key.

---

## 2. The file format

The extension is `.tidemark-provider`. The contents are TOML. Every table below is required
except `[icon]`.

### Top level

| Field | Type | Rule |
|---|---|---|
| `format_version` | integer | Must be `1`. Checked before anything else in the file is looked at, so a future format cannot be half-read by an old build. |

### `[provider]`

| Field | Type | Rule |
|---|---|---|
| `id` | string | Lowercase reverse-DNS, matching `[a-z0-9]+(?:[.-][a-z0-9]+)*`, **with at least one dot**. At most 128 bytes. |
| `name` | string | Display name. At most 128 bytes. |
| `plugin_version` | string | Your own SemVer string. At most 128 bytes. Shown at import and replacement; informational only — `format_version` is what controls compatibility. |

`id` is a **persistent storage key**. Accounts, API keys, history, notification preferences
and the endpoint URL all hang off it. Changing it in a later release of your plugin does not
migrate anything: it creates a second, unrelated provider. Pick it once.

The dot is not decoration. An id without one could collide with a built-in provider slug
that a future Tidemark release adds, and a collision on a storage key means somebody's
history filed under another provider. Ids under `tidemark.*`, the bare id `tidemark`, and
every built-in slug are reserved and refused.

### `[request]`

| Field | Type | Rule |
|---|---|---|
| `method` | string | `"GET"` or `"POST"`. Nothing else. A `POST` always has an empty body — there is no way to send one. |
| `api_key_header` | string | The header Tidemark puts the account's key in. Must be a valid RFC 9110 field name: visible ASCII without separators. |
| `api_key_prefix` | string, optional | Printable ASCII placed in front of the key, verbatim. Usually `"Bearer "` or `""`. Defaults to `""`. At most 128 bytes. |

These header names are refused outright, because each of them changes where the request
goes, how the connection is framed, or who else sees the credential — decisions that belong
to the account owner, not to you:

`Host` · `Cookie` · `Connection` · `Content-Length` · `Transfer-Encoding` ·
`Proxy-Authorization` · `Proxy-Connection` · `TE` · `Trailer` · `Upgrade`

You *may* name `User-Agent` or `Accept` as your key header — neither moves the request. But
Tidemark sets its own `User-Agent` (`Tidemark/<version>`) and `Accept: application/json`
**after** your header and by replacement, so naming one of them means your key is discarded
for that request, not that Tidemark's identity goes out carrying it.

### `[parser]`

| Field | Type | Rule |
|---|---|---|
| `language` | string | Must be `"lua54"`. |
| `source` | string | The Lua chunk. Must contain the text `function parse`. At most 128 KiB. |

Use a TOML literal multiline string (`'''…'''`) so backslashes and quotes in your Lua are
not reinterpreted by the TOML parser.

### `[icon]`

| Field | Type | Rule |
|---|---|---|
| `svg` | string, optional | A static SVG mark. See [§7](#7-the-provider-mark). At most 128 KiB. |

### What the file may not contain

There is no endpoint field, and there will not be one. There is no place to put a host, a
path, a query, a second request, a header other than the one the key goes in, a timeout, a
retry policy or a redirect rule. All of those are Tidemark's, and that is what makes
importing a file from a stranger a reasonable thing to do.

---

## 3. The response: JSON as Lua sees it

Your `parse` receives the decoded response body as its first argument.

| JSON | Lua |
|---|---|
| object | table with string keys |
| array | table as a **one-based sequence** — use `ipairs`, index from `1` |
| string | string |
| integer number | integer |
| non-integer number | float |
| `true` / `false` | boolean |
| `null` | the **`null` sentinel**, not `nil` |

### `null` is not `nil`

Lua cannot tell `nil` from a missing key, and "the provider said the limit is null" and "the
provider did not mention a limit" are different facts you must be able to branch on. So a
JSON `null` arrives as a shared, read-only sentinel table bound to the global `null`.

```lua
local cap = model.limits.requests.amount

if is_null(cap) then
    -- the provider explicitly said: no cap
elseif cap == nil then
    -- the provider did not mention this field at all
else
    -- a real value
end
```

Test it with `is_null(value)`, or compare against the global: `cap == null`. Writing into the
sentinel raises `the null sentinel is read-only`, and its metatable cannot be replaced.

A sentinel is **not** a number. Passing one to `number()` or `percent()` fails with
`number cannot read a table` — the sentinel *is* a table — which is the point: it fails
loudly instead of silently becoming zero.

### The context table

The second argument is a small table. It currently has exactly one field:

| Field | Type | Meaning |
|---|---|---|
| `captured_at` | integer | Unix seconds when this reading was taken. |

`captured_at` is the only clock you get, and it is deliberate: given the same body and the
same `captured_at`, your parser must produce the same reading, on every platform and on
every run. That is what makes `tidemarkctl plugin render` a faithful preview.

The context does **not** contain the account, the endpoint, the key, the request headers, or
anything else about the request.

---

## 4. The sandbox: what you get, and what is gone

The VM is Lua 5.4, built with only three standard libraries and then further trimmed. It is
constructed rather than restricted: a name that is not listed below is not there.

### Standard libraries present

`math` — minus `math.random` and `math.randomseed`
`string` — minus `string.dump`
`table` — whole

### Base functions present

`assert` · `error` · `ipairs` · `select` · `tonumber` · `tostring` · `type` · `_VERSION`

That is the complete list. Every other base name is removed by name — see the table below.

### Host functions

| Function | Returns | Notes |
|---|---|---|
| `number(v)` | number | Accepts a number or a numeric string (whitespace trimmed). A whole value comes back as a Lua **integer**, so `42` prints as `42`, not `42.0`. Refuses a non-finite result, a boolean, a table, `nil` or the `null` sentinel. |
| `percent(value, maximum)` | float | `value / maximum * 100`. Both arguments go through `number()`. Raises if `maximum <= 0` — a provider that reported no maximum has no percentage, and inventing one would put a confident wrong number on a card. Always a float. |
| `parse_time(v)` | integer | Unix seconds. Accepts an RFC 3339 timestamp string, a decimal string of Unix seconds, an integer, or a float (rounded). Anything else raises `parse_time takes RFC 3339 or Unix seconds`. |
| `is_null(v)` | boolean | Whether `v` is the JSON `null` sentinel. |
| `sorted_keys(t)` | sequence | The string keys of a table, sorted, as a one-based sequence. The **only** way to iterate an object's keys. |
| `gauge(id, opts?)` | widget | See [§5](#5-the-reading-metrics-windows-and-layout). |
| `value(id, opts?)` | widget | " |
| `ratio(id, opts?)` | widget | " |
| `status(id, opts?)` | widget | " |
| `null` | table | The JSON-null sentinel itself. |

### Removed, and why

| Removed | Why |
|---|---|
| `pairs`, `next` | Hash-order iteration differs between platforms, and your output order is part of what the plugin publishes. Two machines would draw two different cards from one response. Use `sorted_keys`. |
| `pcall`, `xpcall` | A resource limit is the daemon's verdict, not a condition to recover from. A plugin that could swallow it could loop forever inside a `pcall`. |
| `load`, `loadstring`, `loadfile`, `dofile`, `require` | Loading a chunk at runtime is code that never passed import validation. |
| `io`, `os`, `package`, `debug`, `arg` | The filesystem, the environment, the wall clock, the process. A parser is a pure function; use `context.captured_at` for time. |
| `coroutine` | Nothing in the contract needs one, and the instruction hook accounts for a single stack. |
| `math.random`, `math.randomseed` | Non-determinism. Same input, same output. |
| `string.dump` | Emits bytecode. |
| `getmetatable`, `setmetatable`, `rawget`, `rawset`, `rawequal`, `rawlen` | These are how the `null` sentinel's read-only guard would be taken off. |
| `collectgarbage` | Heap accounting is the host's. |
| `print`, `_G`, `unpack`, `utf8` | No output channel; no global-table reflection; not part of the contract. |

Each execution gets a fresh VM. Nothing is shared between accounts, between polls, or
between the module chunk and a later call — a global you set during one poll is gone by the
next.

---

## 5. The reading: metrics, windows and layout

`parse` must return a table with exactly these three fields:

```lua
return {
    metrics = { … },   -- the numbers
    card    = { … },   -- what the card draws, in order
    details = { … },   -- the detail dialog's sections, in order
}
```

**A reading is accepted or refused whole.** One bad metric does not publish a partial
reading: the account keeps its last good numbers behind a failure chip. Half a reading is
the one outcome that would put a confident wrong number on a card.

### A metric

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | **yes** | Non-empty, unique within the reading, at most 128 bytes. Widgets name a metric by this. |
| `title` | string | **yes** | At most 128 bytes. |
| `subtitle` | string | no | At most 128 bytes. |
| `value` | number | no | What has been consumed. |
| `maximum` | number | no | The cap. |
| `remaining` | number | no | What is left. |
| `used_percent` | number | no | Consumption as a percentage, `0`–`100`. |
| `text` | string | no | Free status text, at most 256 bytes. Read by `status`, and by `value` with `field = "text"`. |
| `unit` | string | no | `"USD"`, `"tokens"`, … At most 128 bytes. |
| `window` | table | no | See below. Turns this metric into a tracked quota window. |

Absent stays absent. Do not put a `0` where the provider reported nothing — a card that
shows a confident zero is worse than one that shows nothing. Every optional field is
genuinely optional.

At most **128** metrics in one reading, and a metric table may not nest deeper than **16**
levels.

### A window

A metric with a `window` becomes a quota window: it gets history, a burn-down chart, pace,
and a notification preference. A metric without one is just a number on the card.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `key` | string | **yes** | The window's **stable identity**, at most 128 bytes. |
| `resets_at` | integer | no | Unix seconds of the next rollover. Use `parse_time`. |
| `length_seconds` | integer | no | The window's length. Must be positive. |

A metric that declares a window **must** also carry a finite `used_percent`, or the whole
reading is refused: a window with no consumption would be drawn as an empty one.

`key` is what history is filed under and what a notification preference names. Changing it in
a later version of your plugin starts a new, empty history. Renaming the metric's *title* is
free; renaming its window key is not.

### Widgets

Four helpers build the card and the detail sections. Each takes a metric id and an optional
table of string options. An unknown option name is an error that names the accepted ones — a
typo does not silently do nothing.

| Widget | Options | Draws |
|---|---|---|
| `gauge(id, opts)` | `field`, `left`, `right`, `format`, `emphasis` | A bar. Uses `used_percent`; if that is absent, `left ÷ right` (default `value ÷ maximum`), which needs a positive denominator. |
| `value(id, opts)` | `field`, `format`, `emphasis` | One number or one string. `field` defaults to `value`. |
| `ratio(id, opts)` | `left`, `right`, `format`, `emphasis` | `left` of `right`. **Both are required.** |
| `status(id, opts)` | `field`, `emphasis` | The metric's `text`. Requires `text` to be present. |

Option values, all strings:

- `field`, `left`, `right` — one of `value`, `maximum`, `remaining`, `used_percent`, `text`
- `format` — one of `number`, `percent`, `currency`, `duration`, `text`
- `emphasis` — `normal` or `compact`

Every widget is checked against the metric it names, at import and at every poll: a `ratio`
missing an operand, a `status` over a metric with no `text`, a `value` reading a field that
is absent, a `gauge` dividing by zero — each is refused with a message naming the metric.

At most **32** widgets on the card, **32** detail sections, and **128** widgets in one
section. A section is `{ title = "…", items = { … } }`; the title is required.

The card order and the detail order are independent, and both are exactly the order you
build them in. Neither is sorted.

### Colours, fonts, spacing

There are none to set. The widgets are semantic: you say what a number *is*, and Tidemark
draws it the way it draws every other provider's. A plugin card and a built-in card read the
same way, which is the point.

---

## 6. Recipes

**A numeric string.** Some endpoints spell money as `"12.5"`. Wrap every number you take from
the response:

```lua
local used = number(period.cost.total)
```

**`used = limit - remaining`.** Common when the endpoint reports only what is left:

```lua
local limit = number(period.requests.limit)
local used = limit - number(period.requests.remaining)
local pct = percent(used, limit)
```

**A field that may be absent.** `nil` is falsy, so a plain guard works:

```lua
local subtitle = nil
if period.plan ~= nil then
    subtitle = tostring(period.plan)
end
```

**A field that may be `null`.** Guard with `is_null` *before* `number()`:

```lua
local cap = model.limits.requests.amount
if not is_null(cap) and cap ~= nil then
    -- safe to call number(cap)
end
```

**Finding one entry in an array.** Arrays are one-based sequences; `ipairs` works:

```lua
local sonnet = nil
for _, model in ipairs(period.models) do
    if model.modelId == "sonnet-5" then
        sonnet = model
        break
    end
end
```

**Iterating an object's keys.** `pairs` is gone. `sorted_keys` gives a deterministic order:

```lua
for _, key in ipairs(sorted_keys(response.buckets)) do
    local bucket = response.buckets[key]
    -- …
end
```

**Adding a metric conditionally.** Build the lists and insert into them; keep the widget
insert next to the metric insert so the two never drift apart:

```lua
local metrics, card = {}, {}
if sonnet ~= nil then
    table.insert(metrics, { id = "sonnet", title = sonnet.modelName, … })
    table.insert(card, value("sonnet", { field = "value" }))
end
```

**A reset time.** RFC 3339 or Unix seconds, both through one function:

```lua
resets_at = parse_time(period.resetsAt)   -- "2026-10-01T00:00:00Z" or 1790812800
```

**A status line instead of a number.** Some endpoints only say "active" or "over quota":

```lua
metrics = { { id = "plan", title = "Plan", text = tostring(response.status) } },
card = { status("plan") },
```

**Failing on purpose.** If the response is not the shape you expect, raise. The account keeps
its last good reading and shows a failure chip; nothing is fabricated:

```lua
if response.lastMonth == nil then
    error("this response has no lastMonth period")
end
```

Your message is shown to the user, cut to 512 bytes. Do not put response content in it that
you would not want in a log.

---

## 7. The provider mark

The optional `[icon] svg` is your provider's mark, drawn beside its name on the card.

It is not checked and kept — it is **parsed, matched against an allowlist and rewritten**.
Only what the allowlist names survives.

**Elements accepted:** `svg` `g` `path` `rect` `circle` `ellipse` `line` `polyline`
`polygon` `title`

**Attributes accepted:** `viewBox` `width` `height` `xmlns` `transform` `d` `x` `y` `x1`
`y1` `x2` `y2` `cx` `cy` `r` `rx` `ry` `points` `fill` `fill-rule` `fill-opacity` `opacity`

Any other attribute is dropped silently. Comments, processing instructions, CDATA and
declarations are dropped. Text is kept only inside `<title>`.

**Fills, never strokes.** Tidemark loads a mark as a *symbolic* icon, and the symbolic loader
forces `fill` on every path to the theme's foreground colour. A `stroke` is not recoloured —
it is not drawn at all. A mark whose geometry is strokes renders as an empty card. Outline
your strokes into filled paths before shipping.

Use `fill="currentColor"`, or no fill at all. A hard-coded colour will simply be overridden.

### What is refused, and the message you get

| Cause | Message |
|---|---|
| `<script>`, `<style>`, `<image>`, `<use>`, `<a>`, `<text>`, `<foreignObject>`, any `<animate…>`, `<set>`, `<filter>`, `<clipPath>`, `<mask>`, `<pattern>`, gradients, `<symbol>`, `<marker>`, `<switch>`, `<iframe>`, `<audio>`, `<video>`, `<handler>`, `<listener>` | `<name> is not allowed in a provider mark` |
| Any other unlisted element | `<name> is not part of the accepted subset` |
| An `on*` attribute, anything containing `href`, any `xlink:*` | `name is not allowed in a provider mark` |
| An accepted attribute whose value contains `url(` or `data:` | `name references something outside the document` |
| `<!DOCTYPE`, `<!ENTITY`, `<?xml-stylesheet` anywhere in the source | `<!doctype is not allowed in a provider mark` (and so on) |
| The root element is not `<svg>` | `the document root must be <svg>` |
| No `<svg>` at all | `no <svg> element was found` |
| Nesting deeper than 32 elements | `nesting deeper than 32 elements` |
| Over 128 KiB | `the provider mark is 〈n〉 bytes, and the limit is 131072` |

All of these are refused rather than quietly stripped, on purpose: a mark that carried a
script is a file whose author's intent is not something to guess at.

A plugin with no mark is a normal, supported configuration. The card simply has no icon.

---

## 8. The authoring loop

Two commands, neither of which installs anything, makes a request, or reads a key.

### `plugin validate`

```bash
tidemarkctl plugin validate path/to/my.tidemark-provider
```

Parses the file, compiles the Lua and sanitizes the mark, then prints what the file declares:

```
would install com.acme.quota
name         Acme AI
version      1.0.0
method       GET
key header   X-Acme-Key
key prefix   (none)
mark         yes
```

Safe to run on a file somebody sent you — that is what it is for. Read the `key header` and
`key prefix` lines before you trust a file: they say exactly where your key will go.

Add `--format json` for a machine-readable version.

### `plugin render`

```bash
tidemarkctl plugin render path/to/my.tidemark-provider --response saved-response.json
```

Runs your parser against a saved JSON body and prints the reading. This is the loop: save one
response to a file, edit the Lua, re-run, look at the output. No key is read and no request is
made.

The card comes first and keeps its order, because that order *is* the answer you are looking
for — it is what the grid will draw.

```
card
  gauge    month-cost           used_percent Monthly cost
  value    month-requests       value        Requests
Limits
  ratio    month-cost                        Monthly cost
  ratio    model-sonnet-5                    Sonnet 5
metrics
  month-cost           Monthly cost                   12.5         50 USD
  month-requests       Requests                       3760       5000
  model-sonnet-5       Sonnet 5                        320       1000
```

`--format json` prints the whole presentation, which is what the GUI receives.

### Then install it

```bash
tidemarkctl plugin install path/to/my.tidemark-provider   # store the definition
tidemarkctl plugin list                                   # what is installed
tidemarkctl provider add com.acme.quota                   # configure it, with a default account
tidemarkctl plugin endpoint com.acme.quota https://…/usage
tidemarkctl auth set-key com.acme.quota                   # key from stdin
tidemarkctl refresh com.acme.quota
tidemarkctl usage --provider com.acme.quota
```

Installing a definition does **not** configure a provider — `plugin install` stores the file,
`provider add` starts polling it. That separation is what lets you inspect an installed
definition without giving it a key.

More accounts on the same plugin, each with its own URL and key:

```bash
tidemarkctl account add com.acme.quota work
tidemarkctl plugin endpoint com.acme.quota https://…/usage --account work
tidemarkctl auth set-key com.acme.quota --account work
```

Plain HTTP needs a per-account acknowledgement, because it puts the key on the network in
clear:

```bash
tidemarkctl plugin endpoint com.acme.quota http://metrics.corp.test/usage --allow-insecure-http
```

### Where things live

| What | Where |
|---|---|
| Installed definitions | `<data dir>/plugins/<id>.tidemark-provider` |
| Materialized marks | `<data dir>/plugins/icons/hicolor/symbolic/apps/tidemark-<id-with-dots-as-dashes>-symbolic.svg` |
| Endpoint and acknowledgement | `config.toml`, under `[provider."<id>".account.<account>"]` as `endpoint` and `allow_insecure_http` |
| The API key | The platform keyring. Never in config, never in the plugin file. |

`<data dir>` is `$XDG_DATA_HOME/tidemark` (typically `~/.local/share/tidemark`), or
`%LOCALAPPDATA%\tidemark` on Windows. `tidemarkctl data` prints the paths this daemon is
actually using.

---

## 9. Security model and limits

### What a plugin cannot do

- **Choose a host.** The account owner supplies the whole absolute URL.
- **See the key.** It is not in `response`, not in `context`, not in any diagnostic, and it
  is redacted out of the raw-response debug log if the endpoint echoes it back.
- **Send a second request**, a body, a cookie, or a header of its own.
- **Follow a redirect.** Redirects are off. A `3xx` is a failed poll, not a hop — so the key
  cannot be moved to another origin by an endpoint that answers `302`.
- **Reach the filesystem, the environment, the clock, the network, the keyring or another
  account.** The sandbox has none of those.
- **Choose colours, fonts or layout.** The widgets are semantic.
- **Recover from a resource limit.** `pcall` is gone.

### What Tidemark owns

- The single request: `GET`, or `POST` with an empty body.
- `User-Agent: Tidemark/<version>` and `Accept: application/json`, set last and by
  replacement, so a declared key header cannot displace or ride along with them.
- The proxy policy — shared with every other provider, and bypassed for loopback. A plugin
  gets no say in it.
- Certificate validation.
- Redirect policy, timeouts, and the response size bound.
- Whether plain HTTP is allowed at all, per account.

### An endpoint is refused if it

- is not an absolute URL;
- has a scheme other than `https` or `http`;
- is `http` without the account's explicit acknowledgement;
- has no host;
- carries credentials (`https://user:pass@host/…`);
- carries a fragment (`…#anything`).

A query string is fine.

### Every limit

Verbatim from [`limits.rs`](../crates/tidemark-core/src/plugin/limits.rs). Exhausting one is
a provider failure — the account keeps its last good reading — never a crash and never a
partially accepted reading.

| Bound | Value |
|---|---|
| The whole `.tidemark-provider` file, Lua and SVG included | 512 KiB (`524288` bytes) |
| The `[parser] source` block alone | 128 KiB (`131072` bytes) |
| The HTTP response body, before JSON parsing | 4 MiB (`4194304` bytes) |
| Lua heap for one execution | 16 MiB (`16777216` bytes) |
| Lua VM instructions per execution | 1,000,000 |
| Metrics in one reading | 128 |
| Widgets on the card | 32 |
| Detail sections | 32 |
| Widgets in one detail section | 128 |
| A metric id | 128 bytes |
| A title, subtitle, section heading, provider name or unit | 128 bytes |
| A metric's status text | 256 bytes |
| A diagnostic excerpt taken from Lua or from a response | 512 bytes |

Two more bounds live with the code that enforces them: the SVG mark is at most **128 KiB**
and may not nest deeper than **32** elements ([`svg.rs`](../crates/tidemark-core/src/plugin/svg.rs)),
and a returned metric table may not nest deeper than **16** levels
([`output.rs`](../crates/tidemark-core/src/plugin/output.rs)).

### Failures, by stage

Every failure names exactly one stage, in this order:

| Stage | Message shape |
|---|---|
| Size | `〈what〉 is 〈n〉 bytes, and the limit is 〈n〉` |
| Not readable (UTF-8, TOML, or JSON) | `〈subject〉 could not be read: 〈reason〉` |
| Format version | `this build reads plugin format 1, and the file declares 〈n〉` |
| Schema | `〈field〉: 〈reason〉` |
| Reserved id | `provider id 〈id〉 is reserved for Tidemark` |
| Forbidden header | `〈header〉 cannot carry a plugin's key` |
| SVG | `the provider mark is not accepted: 〈reason〉` |
| Lua compile | `the parser does not compile: 〈reason〉` |
| Lua runtime | `the parser failed: 〈reason〉` |
| Resource limit | `the parser exceeded its 〈instruction\|memory\|stack〉 limit` |
| Output validation | `the parser returned something unusable: 〈reason〉` |

At poll time every one of these reaches the card as the `malformed` state: the response
arrived and did not mean what the definition says it means. The remedy is to fix the plugin
or ask the endpoint's owner what changed — not to wait, and not to paste a new key.

### Two rules for your own files

- **Never put a real endpoint, API key or recorded live response in a plugin or a fixture.**
  The example in this guide is synthetic, and so is its response.
- **The account owner chooses the URL.** Do not document a "required" host as though the
  plugin depended on it; say what shape of endpoint the parser expects, and let the user
  point it wherever their deployment lives.

---

## 10. Versioning, replacement and removal

**`format_version` controls compatibility.** It is `1`, and it is checked before the file's
Lua is looked at. A build that does not implement a declared version refuses the file whole,
rather than reading the half it recognises.

**`provider.plugin_version` is yours.** SemVer, shown at import and replacement. Tidemark
does not compare it, order it or gate anything on it — it is there so a person can see which
version they are replacing with which.

**Re-importing the same `provider.id` replaces the definition.** Import is transactional: the
new file is validated first, and only then are its exact bytes written. A rejected
replacement leaves the previous definition installed and polling.

What survives a replacement, because it is filed under the id and not under the file:

- configured accounts
- endpoint URLs and their insecure-HTTP acknowledgements
- API keys
- history and its segments
- notification preferences

What a replacement changes: the metadata, the parser, the request declaration and the mark.
Note that a new parser may declare **different window keys** — and a window key is the
identity history is filed under. Changing one starts a fresh, empty history for that window.
Keep your window keys stable across versions unless you mean to reset them.

**Removal is refused while accounts remain.**

```
$ tidemarkctl plugin remove com.acme.quota
org.freedesktop.DBus.Error.InvalidArgs: com.acme.quota still has 1 account(s) configured
```

Remove the accounts first, with the ordinary removal that also deletes their credentials:

```bash
tidemarkctl provider rm com.acme.quota --account work
tidemarkctl provider rm com.acme.quota
tidemarkctl plugin remove com.acme.quota
```

In the GUI a plugin with no accounts shows a trash button on its row in the provider picker.

---

## Checklist

Before you ship a plugin:

- [ ] `format_version = 1`.
- [ ] `provider.id` is lowercase reverse-DNS with at least one dot, and is the id you intend
      to keep forever.
- [ ] `request.api_key_header` is the header the endpoint actually reads, and is not on the
      forbidden list.
- [ ] `api_key_prefix` ends with a space if the endpoint expects one (`"Bearer "`).
- [ ] Every number taken from the response goes through `number()`.
- [ ] Every field that can be `null` is guarded with `is_null` before use.
- [ ] No `pairs`; object iteration goes through `sorted_keys`.
- [ ] Every metric that declares a `window` also has a finite `used_percent`.
- [ ] Window keys are stable and will not change in the next version.
- [ ] No optional field is filled with a fabricated `0`.
- [ ] Every widget names a metric that exists and has the field it reads.
- [ ] The mark is filled geometry — no `stroke` anywhere.
- [ ] `tidemarkctl plugin validate` passes.
- [ ] `tidemarkctl plugin render` produces the card order you meant, against at least one
      saved response with a missing field and one with a `null`.
- [ ] Neither the plugin nor any fixture you distribute contains a real endpoint, a real key,
      or a recorded live response.
