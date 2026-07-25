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

**Complete audit, then every finding fixed and committed one at a time.**

- `c55d795` **apps: UninstallString via CreateProcessW, not `cmd.exe /C`.** cmd
  strips the outer quotes of a `/C` argument and re-parses, so a quoted image
  path containing `&` split into two broken commands. Not hypothetical — this
  box carries `"…\Intel(R) Graphics Software & Drivers\Uninstaller.exe"`. A
  probe reproduced the failure (`exit=1`, `'C:\Program'은(는) … 아닙니다`) and
  the fix runs the same binary cleanly. Elevation-required uninstallers fall
  back to `ShellExecuteW "runas"`.
- `21beefa` **taskmgr: enumeration moved off the UI thread.** Opening the Task
  Manager froze its own window: the cold sample costs ~1.3 s (per-process
  version-resource reads for `FileDescription`) and ran between `ShowWindow`
  and the first paint. A worker owns the `Sampler` and posts `WM_SNAPSHOT`; the
  request is fired right after `CreateWindowExW` so it overlaps D2D and font
  setup. Measured with a `WM_NULL` `SendMessageTimeout` round-trip taken the
  moment the window turns visible — **before 764 ms / 502 ms worst-case, after
  148 ms / 254 ms (both a paint, not a stall), steady state 0.2 ms**.
- `c357fee` **startmenu: pin reordering made total.** The drag reflow rebuilt
  the pin vector with `old[i].take().unwrap()` — a panic on the shell's UI
  thread, which takes the desktop with it. No caller can currently produce a
  bad index, so this is hardening, not a live crash. Both permutations are now
  pure functions (`permute`, `permute_slots`) with **the crate's first tests, 6
  passing**, covering the repeated- and stale-index cases that used to panic.
- `563ed6d` actioncenter: dead `else if on { TEXT } else { TEXT }` arm removed.
- `b8b6dbb` docs: hard-coded home directory in the rollback plan → `%USERPROFILE%`.
- `cfa943c` taskbar: recorded why `secondaries` is `Vec<Box<Secondary>>` — each
  Secondary hands its own address to `GWLP_USERDATA`, so the boxing is
  load-bearing and clippy's `vec_box` advice would be a use-after-move.
- `f596410` **clippy: workspace to zero warnings** (was 67). Substantive fixes
  (identity `map`, redundant `i32` cast ×2, `clamp`, range `contains` ×2,
  `is_none_or`, `let…else` → `?` ×2, a doc line rustdoc read as a list item, a
  complex tuple named `DeviceSection`, and the `&mut STARTUPINFOW` from the
  uninstall rewrite). `collapsible_if` (49 sites) and `too_many_arguments` (4)
  are allowed at the workspace level with the reasoning in `Cargo.toml`:
  `clippy --fix` cannot reindent the bodies it rewrites, and reformatting the
  codebase over a style lint is not a trade worth making.
- `99b72b5` **taskbar: `Bar` boxed** so the pointer in `GWLP_USERDATA` is heap
  owned rather than a stack address. The embedded panels were already sound —
  every one registers from `show()`/`open()`, by which point it sits at its
  final address inside `Bar` — but `Bar` itself had no such moment.
- `9fac1ba` **render: `rect()` and `fill_round()` hoisted out of ten painters**
  (−145/+48). `rect` was byte-identical ten times; `fill_round` had two
  divergent bodies. Each painter keeps a one-line delegate, so no call site
  moved.

Verification: `cargo clippy --workspace --all-targets` silent, `cargo test
--workspace` 6 passed, and the shell was launched and driven end to end.

Every surface the audit touched was captured after the fixes — bar, start menu,
action center, Wi-Fi flyout, volume flyout, Task Manager. All render. The bar
was driven by posting `WM_LBUTTONDOWN`/`UP` into its own window (no global input
injection): Start opened the app list with 초성 sections, the tile grid and its
folder group; the notification cell opened the backlog plus the Quick Settings
grid (the tile foreground touched by `563ed6d`); the tray cells opened the live
SSID scan and the 출력/입력 device panel (the `DeviceSection` alias from
`f596410`). Closed with `WM_CLOSE`, which runs the `ABM_REMOVE` path — the work
area went back from 1536×840 to 1536×912, so the appbar reservation was
released and nothing was left stranded.

One capture artifact, chased down rather than assumed: the first full-screen
shot after launch showed only the desktop window with no bar. A fresh launch
captured clean with no intervention, and the bar carries `WS_EX_TOPMOST`
(`0x8200088`) while the desktop window does not (`0x8200080`), so it was a
startup-timing artifact of the capture, not a z-order bug.

Left undone, deliberately: the audit also suggested extracting the repeated
window scaffold (class registration + wndproc + `GWLP_USERDATA` + D2D setup,
written out eight times). That is a restructuring of every UI module with real
regression risk and no user-visible gain, and win32 windows differ enough in
styles and lifecycle that the shared part would be thin. Not taken.

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
