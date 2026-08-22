# Easy Keyed Provider Ports Implementation Plan

**Goal:** Add the eight remaining CodexBar providers whose entire authentication is a user-supplied API key — or, in one case, no key at all. Nothing here needs a cookie jar, an OAuth flow, a subprocess, or a cloud signer.

**Depends on:** `docs/superpowers/plans/2026-08-21-keyed-provider-ports.md` is complete. Its **Porting Procedure** (steps A–F: read the contract, harvest the fixtures, write the failing tests, write the `Spec`/`parse`, register, verify) is the method for every task below and is not restated here. Its **Global Constraints** apply unchanged.

**Source material:** `~/repos/CodexBar` is gone. Clone upstream fresh into a scratch directory:

```bash
git clone --depth 1 https://github.com/steipete/CodexBar.git
```

Read at `27c7f33` (0.54.1) or later. Contracts live in `Sources/CodexBarCore/Providers/<Name>/`, plugins in `Sources/CodexBarCore/Resources/Plugins/<id>.js`, recorded response bodies in `Tests/CodexBarTests/<Name>*Tests.swift`, and prose in `docs/<slug>.md`.

**Unverified, as before:** none of these has been seen answering. Every test number comes from a body CodexBar recorded, and the tests assert agreement with CodexBar rather than with the live API.

---

## Task 1 — A provider that takes no credential

`Keyed::fetch_inner` rejects a blank credential before spending a request, and `registry::catalog` publishes `CredentialKind::Key` for everything in both tables. Wayfinder is a loopback gateway whose read-only endpoints are unauthenticated: it has a `base_url` and nothing else, and today the settings dialog would demand a key it can never be given.

- [ ] Add `CredentialKind::None` to the wire enum and give `HandSpec` a way to declare it, so `registry::catalog` publishes a provider with a `base_url` option and no key field.
- [ ] The settings dialog draws such a provider without a credential row; adding the account is the option alone.
- [ ] Tests: a definition with `CredentialKind::None` carries no credential hint, and a keyed provider's definition is unchanged.

Only Task 8 consumes this. If Wayfinder is dropped, drop this task with it.

## Tasks 2–9 — One provider each

Each task follows the Porting Procedure. Every one of them also ships, as its definition of done:

- a symbolic mark at `data/icons/hicolor/symbolic/apps/tidemark-<slug>-symbolic.svg`, **drawn as filled outlines — a `stroke` is not drawn at all by GTK's symbolic renderer, and `scripts/check-desktop-integration.sh` rejects one**;
- a row in `docs/TRADEMARKS.md`.

| # | Slug | Shape | Auth | Contract | Fixtures |
|---|---|---|---|---|---|
| 2 | `opencodego` | `Spec`, one GET | Bearer | `GET https://opencode.ai/zen/go/v1/usage` — `OpenCodeGo/OpenCodeGoUsageFetcher.swift`. The local SQLite history and the Zen balance fetcher are **out of scope**: the usage endpoint alone carries the windows. | `OpenCodeGoUsageParserTests.swift` |
| 3 | `moonshot` | `Spec`, one GET | Bearer | `GET /v1/users/me/balance` on `api.moonshot.ai` or `api.moonshot.cn` — region is an `OptionSchema`, exactly as Z.ai's is. `Moonshot/MoonshotUsageFetcher.swift`, `MoonshotRegion.swift` | `MoonshotUsageFetcherTests.swift` |
| 4 | `deepseek` | `Spec`, one GET | Bearer | `GET https://api.deepseek.com/user/balance` — `is_available` plus a `balance_infos` array per currency. The platform-session path (cost, tokens) needs a browser session and is **out of scope**. `DeepSeek/DeepSeekUsageFetcher.swift` | `DeepSeekUsageFetcherTests.swift` |
| 5 | `minimax` | `Spec`, one GET | Bearer (`sk-cp-*`) | The Coding Plan API-token path **only** — `MiniMax/MiniMaxUsageFetcher.swift`, region via `MiniMaxAPIRegion.swift` (`api.minimax.io` / `api.minimaxi.com`). The cookie, localStorage and web-session paths that make up most of that 5 000-line module are **out of scope**; do not port the auto-fallback between them. | `MiniMaxAPITokenFetchTests.swift`, `MiniMaxCurrentTokenPlanResponseTests.swift` |
| 6 | `deepgram` | `HandSpec`, two GETs | `Authorization: Token <key>` | `GET {base}/projects` → `GET {base}/projects/{id}/usage/breakdown`. The JS plugin `Plugins/deepgram.js` is the whole contract in one file, including its status classification. Project selection is an `OptionSchema`; absent, take the first project. | `DeepgramProviderTests.swift` |
| 7 | `codebuff` | `HandSpec`, POST + GET | Bearer | `POST https://www.codebuff.com/api/v1/usage` and `GET /api/user/subscription` — `Codebuff/CodebuffUsageFetcher.swift`. The CLI credentials-file source is **out of scope**; the key comes from the Secret Service like every other. | `CodebuffUsageFetcherTests.swift` |
| 8 | `kilo` | `HandSpec`, two requests | Bearer | `https://app.kilo.ai/api/trpc` and `GET https://api.kilo.ai/api/profile` — `Kilo/KiloUsageFetcher.swift`. The `~/.local/share/kilo/auth.json` CLI fallback and its `auto` source mode are **out of scope**. | `KiloUsageFetcherTests.swift` |
| 9 | `wayfinder` | `HandSpec`, three GETs, no credential | none | `GET {base}/healthz`, `/router/models`, `/v1/savings` against a loopback gateway, default `http://127.0.0.1:8088`, `base_url` required. `Wayfinder/WayfinderUsageFetcher.swift`. **Needs Task 1.** | `WayfinderProviderTests.swift` |

## Two of these cards will be empty, and that is the honest answer

Moonshot and DeepSeek report a credit balance with no limit behind it. The keyed-provider design already rules on this: a balance with no limit emits no window and only a `DetailSection`, so the card renders empty and sorts last under `model::compare`. Do not invent a limit to draw a bar against. If an empty card is judged unacceptable, that is separate work — a card that draws a balance without a bar — and it is not in this plan.

## Sequencing

Tasks 2–7 are independent; do them in table order. Task 8 (Kilo) is the largest of the hand-written ones. Task 1 before Task 9, or drop both.

## Out of scope, decided

- **doubao** — its API-key path yields only Ark request limits; the Coding and Agent plan quotas that make the card worth having come from the `arkcli` subprocess. Excluded by the owner.
- **azureopenai**, **ollama** — read and refused during the first port; see the "Ported" section of `docs/superpowers/specs/2026-08-21-keyed-provider-port-design.md`. Neither has usage on the wire.
- **devin** — one Bearer and one GET, but the token is captured from a browser request and expires. It belongs with the session-backed providers, not here.
- The thirty cookie-, OAuth-, CLI- and signer-backed providers. Each needs a mechanism Tidemark does not have.
