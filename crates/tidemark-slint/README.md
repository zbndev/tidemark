# tidemark-slint — Slint client prototype

The main window of the desktop client, drawn with Slint instead of GTK/libadwaita. The
daemon, the D-Bus contract and the decisions about what a card says are the GTK client's:
`model.rs`, `format.rs` and `update.rs` are compiled in from `crates/tidemark/src`
unchanged, and `src/view.rs` holds the pure half of the GTK `card.rs`.

```bash
cargo run -p tidemark-slint              # against the running tidemarkd
SLINT_BACKEND=winit-software cargo run -p tidemark-slint  # compare renderers
```

## In the prototype

- Connection, reconnect and bus-name watch; waiting and welcome pages.
- Cards: mark, name, account caption, plan pill, state chip, headline and bar with pace
  mark, blocked windows with a padlock, reset line, absolutes, secondary rows, wallet
  balance, balance-only and blank cards, footer; redrawn every 30 s.
- Grid: 300 px cells, Auto or capped columns from preferences, centred, uniform height.
- Hover lift; drag to reorder with displaced cards moving out of the way, settle on
  release, autoscroll near the edges; provider and account reorders sent to the daemon,
  optimistic, rolled back on refusal.
- Account groups: `+N` / `−` badge, accounts slide out from and back under their provider.
- Refresh, update button (opens the release page), theme preference, and the desktop's
  dark style and accent from the XDG settings portal.
- Bundled Rubik and embedded symbolic icons, so nothing depends on a system theme.

## Not yet

Provider settings, preferences, about and detail dialogs; card context menu; release
notes preview and the restart prompt; tray; close-to-tray; Windows connection (p2p
endpoint, daemon spawn, single instance).
