# Codex: multiple CLI homes and Business plans

## Multiple Codex CLIs on one machine

The Codex CLI keeps one login per CODEX_HOME (default ~/.codex). Tidemark's default
Codex account reads that file (or a Tidemark login in the keyring).

A **second** CLI login needs its own home directory — the usual pattern is:

`ash
CODEX_HOME=/path/to/second-codex-home codex login
`

Then in Tidemark:

1. Add a second Codex account in Provider settings.
2. Set **CLI home** on that account to the absolute path of the second home.
3. Choose **Codex CLI login** as the credential (or leave Auto).

Without a CLI home, extra Codex accounts remain **Tidemark-login only** — there is still
only one default ~/.codex/auth.json on disk.

## Codex Business (Standard / Premium)

Business seats often return **no** 
ate_limit windows from
https://chatgpt.com/backend-api/wham/usage. The quota lives under
spend_control.individual_limit (used_percent, and often string limit/used).

Tidemark maps that spend-control payload into a Spend window so the card shows usage.

### Upstream CLI limits (not fixable here)

- The official Codex CLI historically used a **closed** plan_type enum; unknown variants
  (and some Business / usage-based plan slugs) can make the CLI fail to decode /wham/usage
  even when the HTTP response is fine. Prefer a current CLI build; see
  [openai/codex#17353](https://github.com/openai/codex/issues/17353) and related plan-type PRs.
- Some Business workspaces expose monthly spend caps instead of the Plus/Pro 5-hour + weekly
  windows. The CLI status UI may disagree with the web usage dashboard
  ([openai/codex#16909](https://github.com/openai/codex/issues/16909)).
- Seat / credit preload and workspace entitlement issues are account-side; Tidemark can only
  display what /wham/usage returns for the token it holds.
