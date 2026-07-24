# glint devlog

Running development log, newest first. Milestone-level architecture and the swap
plan live in [`SHELL_DESIGN.md`](SHELL_DESIGN.md) §9; this file is the
chronological record of what shipped and what was verified, session by session.

Convention: one dated section per working session. Note the commits, what was
built, and — since these are visual projects — **how it was verified** (a green
build is not evidence that anything rendered).

---

## 2026-07-25

**Public-repo setup.** Added this devlog, a `README.md` landing page describing
the workspace and `glide-shell`'s scope, and set the GitHub repository
description and topics. Secret-scanned the tracked tree before publishing —
clean (all `token` hits are Win32 access-token handles, and the rescue-account
script deliberately never scripts a password). No license granted (owner
decision): source is public for reading, all rights reserved.

Established a devlog cadence — this log is updated every session from here on,
not only at milestone boundaries.

## 2026-07-24

**Settings inline controls + native Task Manager** (`b237c93`, log `d07ec6b`).

- **Settings** — the classic panels that used to throw the user back to
  `ms-settings:` now render as real glide controls: 소리 (volume / mute / output
  device via Core Audio `IAudioEndpointVolume`), 전원 (battery + power plan via
  `GetSystemPowerStatus` + powrprof, no elevation), 네트워크 (Wi-Fi / Bluetooth /
  airplane — the WinRT `Radios` calls run on a short-lived MTA worker to avoid
  deadlocking the STA UI thread), 앱 (installed-app enumeration across the three
  Uninstall hives + uninstall), and 날짜·시간 (timezone via
  `SetDynamicTimeZoneInformation`). Genuine admin consoles (device manager, disk
  management, registry editor) stay as launchers — reimplementing them would be
  slop, and Win11 Settings + KDE/GNOME launch them too.
- **Task Manager** (`--taskmgr`, also opened from the settings entry) — modelled
  on the Windows 11 process view: applications grouped **by product name**
  (FileDescription from the version resource) with process counts, collapsible
  groups, and summed CPU/memory; per-app icons (reusing `icons::exe_icon` with a
  path-keyed cache); live search; proportional CPU/memory heat bars; an owner
  column; and two-step terminate / kill-tree / suspend / resume / priority
  (group actions apply to every member). Services tab lists the SCM with
  start/stop.
- **Bug found and fixed mid-session:** the first Task Manager cut grouped by
  parent-process subtree, which made `explorer.exe` swallow every user-launched
  process into one "Windows 탐색기 (55) — 2.8 GB" row. Switched to product-name
  grouping; Explorer drops back to its real ~133 MB.
- New backend modules: `procs`, `services`, `apps`, `datetime`, `power`.
- **Verified** by foreground-launching the windows (user-authorized) and
  screen-BitBlt screenshotting them five times across iterations — DirectComposition
  precludes offscreen capture, so the rig runs in Windows PowerShell 5.1
  (`pwsh` 7 can't reference `System.Drawing`), forces the window topmost, and
  `CopyFromScreen`s its rect. Confirmed grouping, icons, heat bars, column
  alignment, and the Explorer memory correction on screen.

## Earlier milestones (backfilled)

Condensed; see `SHELL_DESIGN.md` §9 for full per-slice detail and verification.

- **2026-07-23** — Settings became a system control center. Dropped the
  `ms-settings:` billboard for a glide-native spine; folded in accent color,
  clock format, toast + autostart toggles, and a taskbar-density picker
  (`35a2988` · `6e867a0` · `367975f` · `9a32f44` · `cdfd655`). Added
  `winsettings.rs` (personalization / startup-programs / system-info, all HKCU,
  no elevation) and a control-center settings app (`d1500a7`). Imported
  Explorer's taskbar pins via `IShellLink` (`4ab38e2`). Floated the secondary
  bar to match the primary (`18d900c`). Fixed the Start menu's 5–6 second
  first-open freeze by pre-warming the cold `shell:AppsFolder` enumeration
  (`0b525fa`).
- **M1–M6** — the alongside-Explorer shell: taskbar + desktop + Start menu with
  drag-and-drop (M1–M3); toast center via `UserNotificationListener` + volume /
  brightness OSDs (M4); per-monitor secondary bar (M5); crash-loop safety ladder
  + rescue account (M6, `safety.rs`); a settings app + Win-key routing (bare
  Win → Start, Win+S → search); a Win10-style unified notification center; a
  tray-overflow chevron; and the floating rounded-panel design language.
- **M7 (pending)** — the swap: rollback rehearsal → flip `Winlogon\Shell` →
  daily-dogfood checklist (Korean IME, DPI, Duo panel detach, sleep/resume,
  fullscreen games, real-app tray re-registration). Gated on the checklist; the
  one unproven path is real-app tray routing, which only an Explorer-kill session
  can exercise.
