<div align="center">

<img src="data/icons/hicolor/512x512@2/apps/io.github.zbndev.Tidemark.png" width="160" alt="Vibe Tavern" />

# Tidemark

**See how much of your AI quota is left — on your Linux desktop.**

![Release](https://www.shieldcn.dev/github/release/zbndev/tidemark.svg?size=sm&theme=zinc)
![GitHub Downloads](https://shieldcn.dev/github/downloads/zbndev/tidemark.svg?variant=secondary)
![GitHub Stars](https://www.shieldcn.dev/github/stars/zbndev/tidemark.svg?variant=secondary&size=sm&theme=zinc)

</div>

---

https://github.com/user-attachments/assets/ecd808c0-7b35-4127-b3e6-ee50d0278e2b

---

Tidemark shows every rate-limit window your AI providers report: how much you have burned,
when it resets, and whether your current pace gets you there. One card per account, side by
side, so you can tell at a glance which provider to start a long run on.

A background service keeps polling while the window is closed, so the tray icon and the
notifications stay current. Native GTK4 + libadwaita — no Electron, no embedded browser.

- **Every window.** Five-hour, weekly, monthly — whatever the provider
  exposes, each with its own reset time.
- **Several accounts per provider.** A work and a personal Claude, two Z.ai keys — grouped
  behind an expand toggle.
- **A pace mark on every bar.** Fill to the left of the mark means the quota likely lasts until
  the reset; fill to the right means it does not.
- **Warnings at 70% and 90%,** plus a notification when a window resets. Off by default,
  switched on per window so you only hear about the ones you care about.
- **History and a burn-down chart.** Click a card to see how the current window was spent.
- **Lives in the tray.** Closing the window hides it; readings keep arriving.

## Installation

### Ubuntu/Fedora

Download the `.deb` or `.rpm` from the [latest release](https://github.com/zbndev/tidemark/releases/latest):

### Arch Linux

Install from AUR with yay/paru

```bash
yay -S tidemark-git
```

### Windows

Experimental. Download the setup executable from the
[latest release](https://github.com/zbndev/tidemark/releases/latest). It installs for the
current user only and needs no administrator rights. The installer is not code-signed, so
SmartScreen stops the first run — More info → Run anyway.

### Nix

Install Tidemark directly from this repository:

```bash
nix profile install github:zbndev/tidemark
```

Or run the window without installing it:

```bash
nix run github:zbndev/tidemark
```

For NixOS, add Tidemark as a flake input, import its module, and enable the service:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    tidemark.url = "github:zbndev/tidemark";
  };

  outputs = { nixpkgs, tidemark, ... }: {
    nixosConfigurations.my-host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        tidemark.nixosModules.default
        { services.tidemark.enable = true; }
      ];
    };
  };
}
```

The module installs Tidemark and registers its D-Bus-activated user daemon. It does not
start the GTK window at login. `nix run github:zbndev/tidemark#tidemarkd` is available for
diagnostics, but normal use should let D-Bus activate the daemon when the window or another
client asks for it.

## Getting started

Open Tidemark. With nothing configured yet it says *Add a provider to start tracking your
quota*.

1. Open provider settings and press **+**.
2. Pick your provider from the searchable list.
3. Sign in, or paste an API key — the detail page says where to find it.

Keys are stored in your desktop keyring, never in a config file. Claude, Codex and
Antigravity can sign in through Tidemark, or reuse the login their own CLI already has.

Removing a provider deletes its Tidemark-owned credentials and its card but keeps the quota
history. A vendor CLI's own credential file remains owned by that CLI and is never removed.

## From the command line

`tidemarkctl` talks to the same daemon the window does, so anything the interface can do is
scriptable — and a panel widget needs no D-Bus code of its own.

```bash
tidemarkctl usage                      # every account, for a person
tidemarkctl usage --format json        # {"accounts": [...]}, for a program
tidemarkctl guard --min-remaining 20 --window w604800 || echo "not this week"
tidemarkctl watch                      # one JSON object per change, until you stop it
```

`guard` exits `0` when the quota is there, `1` when it is not, and `69` when there is no
trustworthy reading to judge — a rejected credential never answers "safe".

Secrets are read from stdin, never from the command line:

```bash
printf '%s' "$ZAI_API_KEY" | tidemarkctl auth set-key zai
```

Commands that act on one existing account use `default` unless `--account work` says
otherwise. Aggregate commands keep their broader meaning: `usage`, `guard` and `watch`
still include every matching account when no account filter is given.

A newly added Claude, Codex or Antigravity account starts pinned to Tidemark OAuth; adding
it never silently adopts the vendor CLI's login:

```bash
tidemarkctl provider add codex
tidemarkctl auth login codex

# Or explicitly use the Codex CLI login that already exists:
tidemarkctl auth select codex --mode cli
```

A Waybar module is two files. In `~/.config/waybar/config.jsonc`:

```jsonc
"custom/tidemark": {
    "exec": "tidemarkctl watch --format waybar",
    "return-type": "json",
    "on-click": "tidemark"
}
```

and in your stylesheet, the classes it sets — `ok`, `warning`, `danger` at the same 70% and
90% the app's own bar changes colour at, plus `stale` when the reading is not fresh:

```css
#custom-tidemark.warning { color: @warning_color; }
#custom-tidemark.danger  { color: @error_color; }
#custom-tidemark.stale   { opacity: 0.5; }
```

`tidemarkctl completions zsh > ~/.zfunc/_tidemarkctl` installs completions;
`tidemarkctl --help` lists the rest — `provider`, `account`, `auth`, `notify`, `config`,
`history`.

## Reporting a wrong reading

If a card shows a number the provider's own dashboard disagrees with, the useful evidence
is the response Tidemark actually received. Put this in `~/.config/tidemark/config.toml`:

```toml
[debug]
raw_responses = true
```

then `systemctl --user restart tidemarkd`. Every provider response is written verbatim,
one JSON object per line, to `~/.local/share/tidemark/debug/responses.ndjson`:

```bash
jq 'select(.provider == "opencodego")' ~/.local/share/tidemark/debug/responses.ndjson
```

API keys are never written: request headers are left out entirely, URL query strings are
redacted, and the sign-in endpoints are not logged at all. The file still describes your
account's usage, so read it before attaching it to an issue. It rolls over at 16 MB and
keeps one previous file. There is no switch in the interface — turning it off is the same
edit, and a restart.

## Supported providers

**Sign in with your account**

Antigravity · Claude · Codex

**Paste an API key**

ai& · Amp · Chutes · ClawRouter · ClinePass · Codebuff · Crof · Deepgram · DeepInfra · DeepSeek ·
ElevenLabs · Factory · Fireworks · Groq · IBM Bob · Kilo · Kimi · LiteLLM · LLM Proxy · MiniMax ·
Moonshot · NanoGPT · Neuralwatt · OpenAI · OpenCode Go · OpenRouter · Poe · StepFun · sub2api ·
Synthetic · Venice · Warp · xAI · Z.ai · ZenMux

**Local session**

Abacus · Alibaba Coding Plan · Augment · CommandCode · Cursor · Gemini · Grok · LongCat · Manus · MiMo · Mistral · Notion · Ollama · OpenCode · Perplexity · Qoder · Sakana · T3 Chat · ZoomMate

**No credential needed**

Wayfinder

Some providers need one extra setting alongside the key — a region, an account id, or the
base URL of your own deployment. The provider's page asks for it.

**Something else**

If the service you use is not listed, you can teach Tidemark to read it without waiting for
a release. A provider plugin is one `.tidemark-provider` file — metadata, a sandboxed Lua
transformation and an optional mark — that you import from **Providers → Import a provider
file**, then point at your own endpoint with your own key. The file cannot name a host and
never sees the key. See [`docs/plugin-providers.md`](docs/plugin-providers.md) for the
format, the sandbox and a worked example.

## Requirements

GTK 4.22 and libadwaita 1.9, which means **Fedora 44+** or **Ubuntu 26.04 LTS+** and their
derivatives. Older distributions cannot run it. Arch and other rolling releases are fine.

## Building from source

Needs Rust 1.92 or newer and the development packages for GTK4, libadwaita and SQLite:

```bash
git clone https://github.com/zbndev/tidemark.git
```

```bash
cargo build --workspace
```

```bash
cargo run -p tidemark
```

`tidemarkd` does the polling and `tidemark` is the window. The window never talks to a
provider itself — it only shows what the daemon publishes on D-Bus, which also makes the
daemon usable from `busctl` or a Waybar module. See [`CONTEXT.md`](CONTEXT.md) for the
architecture and the design record.

## License

MIT — see [`LICENSE`](LICENSE).

The provider marks under `data/icons` are their owners' trademarks and are not covered by
this licence; see [`docs/TRADEMARKS.md`](docs/TRADEMARKS.md), which also explains how to
build without them.

Provider protocol details are taken from [CodexBar](https://github.com/steipete/CodexBar)
(MIT).
