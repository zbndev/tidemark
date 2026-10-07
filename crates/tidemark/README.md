# tidemark — the desktop client

The desktop client, drawn with Slint (FemtoVG: OpenGL on Linux, wgpu on Direct3D 12 on
Windows). It speaks to `tidemarkd` over D-Bus and nothing else. It replaced the GTK and
libadwaita client, whose look it keeps: `ui/theme.slint` and `ui/adw.slint` spell out
libadwaita's palette and widgets.

```bash
cargo run -p tidemark                                # against the running tidemarkd
SLINT_BACKEND=winit-software cargo run -p tidemark   # compare renderers
cargo run -p tidemark --example mock-daemon          # invented accounts; stop tidemarkd first
```

On Windows, build `tidemark` and `tidemarkd` together: the client looks for
`tidemarkd.exe` beside itself and starts it when nothing serves the endpoint. Logs go to
`%LOCALAPPDATA%\tidemark\logs\ui.log`.

## What it does

- Connection, reconnect and bus-name watch; waiting and welcome pages.
- One client per session: on Linux it owns `io.github.zbndev.Tidemark` on the session bus
  and serves `org.freedesktop.Application`, so a second launch raises the first; on
  Windows a session mutex, with activation forwarded through the daemon. `--background`
  (the session autostart) keeps the window hidden, and exits when no panel takes the icon.
  Linux launch tokens reach the first window so the desktop can finish startup
  notification. They are captured before thread setup, cleared from the environment,
  and retained for forwarding when another client already owns the application ID.
- Tray: ksni on Linux, tray-icon on Windows. Accounts with their shortest window's
  percentage in card order, Open, Refresh, Quit; attention at 90%. Closing the window
  hides it while the icon is up and the preference asks for it, and ends the program
  otherwise.
- The window's own header and controls on Linux and Windows, with native dragging and
  resizing and no additional system title bar. Floating windows have transparent rounded
  corners; maximized and full-screen windows have square corners. `WindowMoveArea`
  hands dragging to the system without retaining Slint's pointer grab. Windows uses
  its native shadow; on Wayland a synchronized, click-through subsurface paints the
  shadow below the content, outside Winit's window geometry. It is removed when
  maximized or full-screen and destroyed before hiding in the tray. X11 shadows
  follow the compositor's policy.
- Windows: the p2p endpoint, spawning the daemon into a kill-on-close job, file log,
  no console window.
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
- Card context menu with icons: add an account, modify, remove — shortcuts into provider
  settings. Each configured row's trailing menu offers the same actions; an unconfigured
  plugin's menu adds its first account or removes its file.
- Primary menu with Preferences (General, Network and Data pages, every value the
  daemon's, a refused change put back) and About (details, issue link, legal, and a
  troubleshooting page with the daemon's version, the renderer, the desktop and session,
  whether the tray was accepted, copied or saved through the file chooser).

## Not yet

The GTK client had these; they are still to be drawn here: the detail (history chart)
dialog; release notes preview and the restart prompt; keyboard
focus on cards, and F10 for the primary menu.
