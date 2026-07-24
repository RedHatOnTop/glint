# glint

A **complete desktop environment for Windows**, in the spirit of KDE Plasma or
GNOME — but native. glint keeps the Windows kernel, driver stack, and DWM
compositor, and replaces everything *above* the compositor that makes a desktop
a desktop: the bar, the launcher, the notification and quick-settings surfaces,
the desktop, the session controls, and the settings.

No Electron, no WebView, no bundled Chromium. The shell renders on
Direct2D / DirectWrite / DirectComposition and talks to Win32 directly, so it
starts instantly and idles in tens of megabytes.

> Status: pre-swap. The shell runs today *alongside* Explorer for daily
> dogfooding; the milestone that flips `Winlogon\Shell` to make glint the
> resident shell (M7) is gated on the checklist in
> [`docs/SHELL_DESIGN.md`](docs/SHELL_DESIGN.md).

## Workspace

glint is a Cargo workspace of four crates. The desktop environment is
`glide-shell`; the others are the launcher and file-manager it drives.

| Crate | Role |
| --- | --- |
| **`glide-shell`** | The desktop environment — taskbar, system tray, Start menu, desktop + icons, notification center, quick settings, OSDs, per-monitor secondary bar, a control-center settings app, and a native task manager. Full shell-replacement safety ladder included. |
| **`glide`** | Dual-pane, Win11-native file manager — the Explorer challenger. |
| **`glint`** | Unified launcher for files, apps, and the web. |
| **`glint-core`** | Shared Win11-native building blocks (icon extraction, platform shims, search sources). |

## What `glide-shell` ships

- **Taskbar** — appbar-registered bottom bar with running-window buttons, pinned
  apps imported from Explorer's own taskbar pins, a live system tray (full
  `Shell_NotifyIcon` / appbar relay protocol), clock, and a floating
  rounded-panel design language.
- **Start menu** — apps enumerated from `shell:AppsFolder`, pinned tiles, power
  controls (lock / sleep / restart / shutdown).
- **Desktop** — wallpaper, icons, and mouse **drag-and-drop** arrangement.
- **Notifications & Quick Settings** — a Win10-style notification center driven
  by `UserNotificationListener`, plus a quick-settings flyout (Wi-Fi, Bluetooth,
  airplane) and volume / brightness OSDs.
- **Dual-screen** — a per-monitor secondary bar (built for the ASUS Zenbook Duo).
- **Settings** — a glide-rendered control center: personalization, taskbar,
  startup programs, Windows tweaks, sound / power / network / apps / date-time
  controls rendered inline (not thrown back to `ms-settings:`), with true admin
  consoles kept as launchers.
- **Task Manager** — a native process/service manager modelled on the Windows 11
  view: per-application groups by product name, app icons, live search,
  CPU/memory heat bars, and terminate / kill-tree / suspend / resume / priority.
- **Safety** — a crash-loop ladder that restores the Explorer shell and drops a
  rescue account before glint can lock you out.

## Building

Requires a recent stable Rust (the workspace is edition 2024) and the Windows
10/11 SDK toolchain.

```sh
# the whole workspace
cargo build --release

# just the desktop environment
cargo build -p glide-shell --release
```

Run the shell alongside Explorer (safe — it stacks above the stock bar and never
touches `Winlogon\Shell`):

```sh
target/release/glide-shell.exe
```

Standalone windows for development:

```sh
glide-shell.exe --settings    # the control-center settings app
glide-shell.exe --taskmgr     # the native task manager
```

Turning glint into the *resident* shell is a deliberate, reversible step —
`--register` / `--unregister` manage the `Winlogon\Shell` swap, and the crash
ladder self-restores Explorer. Do not swap until you have read
[`docs/SHELL_DESIGN.md`](docs/SHELL_DESIGN.md).

## Design & log

- [`docs/SHELL_DESIGN.md`](docs/SHELL_DESIGN.md) — the architecture, the shell
  protocols (appbar / tray / shell hook), the safety model, and the milestone
  plan.
- [`docs/DEVLOG.md`](docs/DEVLOG.md) — running development log, newest first.

## License

No license is granted. The source is public for reading; all rights are
reserved. If you want to use any of it, open an issue and ask.
