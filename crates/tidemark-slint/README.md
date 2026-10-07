# tidemark-slint — the Slint desktop client

The desktop client drawn with Slint, replacing the GTK/libadwaita one in `crates/tidemark`,
which goes once this reaches parity. The daemon and the D-Bus contract are unchanged.
`model.rs`, `format.rs`, `update.rs` and the Windows modules (`daemon_job.rs`,
`single_instance.rs`, `file_log.rs`, the reconnect protocol in `bus.rs`) started as copies
of the GTK client's and are this crate's own now; `src/view.rs` holds the pure half of the
GTK `card.rs`.

```bash
cargo run -p tidemark-slint              # against the running tidemarkd
SLINT_BACKEND=winit-software cargo run -p tidemark-slint  # compare renderers
```

On Windows, build `tidemark-slint` and `tidemarkd` together: the client looks for
`tidemarkd.exe` beside itself and starts it when nothing serves the endpoint. Logs go to
`%LOCALAPPDATA%\tidemark\logs\ui.log`.

## In the prototype

- Connection, reconnect and bus-name watch; waiting and welcome pages.
- Windows: the p2p endpoint, spawning the daemon into a kill-on-close job, one client per
  session with a second launch raising the first, file log, no console window.
- Cards: mark, name, account caption, plan pill, state chip, headline and bar with pace
  mark, blocked windows with a padlock, reset line, absolutes, secondary rows, wallet
  balance, balance-only and blank cards, footer; redrawn every 30 s.
- Grid: 300 px cells, Auto or capped columns from preferences, centred, uniform height.
- Hover lift; drag to reorder with displaced cards moving out of the way, settle on
  release, autoscroll near the edges; provider and account reorders sent to the daemon,
  optimistic, rolled back on refusal.
- Account groups: `+N` / `−` badge, accounts slide out from and back under their provider.
- Refresh, update button (opens the release page), theme preference, and the desktop's
  dark style and accent from the XDG settings portal, or the registry on Windows.
- Bundled Rubik and embedded symbolic icons, so nothing depends on a system theme.
- Provider settings, drawn inside the window: configured and installable providers, account
  add/rename/remove, keys, OAuth sign-in, local sources and browser sessions, options and
  notifications, plugin import (file chooser through `rfd`, the XDG portal on Linux) and
  removal; alerts awaited as futures.

## Not yet

Preferences, about and detail dialogs; card context menu; release
notes preview and the restart prompt; tray; close-to-tray; `--background` start;
packaging.
