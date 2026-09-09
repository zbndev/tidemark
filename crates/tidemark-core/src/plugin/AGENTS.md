# PLUGIN KNOWLEDGE BASE

## OVERVIEW
The `.tidemark-provider` format: file parsing, SVG sanitizing, the Lua 5.4 sandbox, output validation, and the one generic single-request provider built on them. Everything here is pure except `provider.rs`'s single HTTP request. The author-facing contract is `docs/plugin-providers.md`; this file is the maintainer's half.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| File format, field rules, reserved ids, forbidden headers | `schema.rs` | `parse(bytes, reserved) -> Definition`, `valid_id` |
| Every bound the runtime enforces | `limits.rs` | Documented verbatim in the author guide |
| Mark allowlist, hostile elements, rewriting | `svg.rs` | `sanitize` returns canonical bytes, not the source |
| Sandbox construction and resource hooks | `lua/mod.rs` | `sandbox`, `run`, `compile`, failure classification |
| Host globals a plugin may call | `lua/api.rs` | `number`, `percent`, `parse_time`, `is_null`, `sorted_keys`, four widget helpers |
| JSON-to-Lua mapping and the null sentinel | `lua/json.rs` | `to_lua`, `install_null`, `is_null` |
| Lua return value to typed reading | `output.rs` | `validate`, per-widget drawability checks |
| Request declaration, endpoint rules, polling | `provider.rs` | `PluginProvider`, `Endpoint`, `build_request`, `poll`, `render` |
| Failure vocabulary | `mod.rs` | `PluginError`, one variant per stage |
| Transport coverage | `../../../tests/plugin_provider.rs`, `../../../tests/plugin_proxy.rs` | Socket-level; proxy is a binary of its own |

## CONVENTIONS
- `PluginError` names exactly one stage; the stage order is size, UTF-8, TOML, `format_version`, then everything else.
- Every plugin failure reaches the engine as `ProviderError::Malformed` — the response arrived and did not mean what the definition says.
- `render(definition, response, account, captured_at)` is the one transformation: polling, `tidemarkctl plugin render` and the tests all call it. Never write a second.
- The sandbox is built by allowlist and then trimmed by name, so a future `mlua` cannot reintroduce a library silently.
- `pairs`/`next` are removed because iteration order is published output; `sorted_keys` is the replacement. `pcall` is removed so a limit cannot be swallowed.
- A reading is accepted or refused whole. No partial publication.
- The sanitized mark is what reaches `Definition.icon_svg`; the author's original bytes stay in `Definition.bytes`.
- Diagnostics are cut to `MAX_ERROR_BYTES` on a char boundary: a plugin's `error()` text is data an endpoint may have influenced.
- `User-Agent` and `Accept` are `insert`ed after the declared key header, so a file may name either and lose rather than leak.

## ANTI-PATTERNS
- Never let the credential reach Lua, a diagnostic, `Debug`, or the debug recorder's stored body.
- Never add an endpoint, host, path or second-request field to the format: the account owner chooses the URL.
- Never accept a redirect: a `3xx` is a failed poll, not a hop for the secret header.
- Never fabricate a metric field the plugin omitted, and never default `used_percent` to zero.
- Never rename a plugin provider id or a window key on the plugin's behalf; both are persistent storage keys.
- Do not add a Lua global without adding it to the guide's global list and to the removal reasoning.
- Do not put a real endpoint, key or recorded live response in a fixture or an example.

## COMMANDS
```bash
cargo test -p tidemark-core plugin
cargo test -p tidemark-core --test plugin_provider --test plugin_proxy
tidemarkctl plugin validate examples/plugins/acme-quota.tidemark-provider
tidemarkctl plugin render examples/plugins/acme-quota.tidemark-provider \
  --response examples/plugins/acme-response.json
scripts/check-plugin-size.sh <baseline-commit>
```

## NOTES
- Installation, the on-disk store, the materialized mark and the endpoint live in `tidemarkd` (`plugins.rs`, `registry.rs`, `service.rs`), not here.
- `svg.rs` carries two bounds of its own (128 KiB, 32 levels of nesting) and `output.rs` one (16 levels of table nesting); the rest are in `limits.rs`.
- The example plugin and response under `examples/plugins/` are copies of the test fixtures in `../../../tests/fixtures/plugin/`. Change both together.
