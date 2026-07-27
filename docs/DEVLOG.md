# glint devlog

Running development log, newest first. Milestone-level architecture and the swap
plan live in [`SHELL_DESIGN.md`](SHELL_DESIGN.md) §9; this file is the
chronological record of what shipped and what was verified, session by session.

Convention: one dated section per working session. Note the commits, what was
built, and — since these are visual projects — **how it was verified** (a green
build is not evidence that anything rendered).

---

## 2026-07-27 (evening)

Desktop gap 1 of 4: **the desktop had no namespace items.** It enumerated two
folders and nothing else, so 휴지통 — the one desktop icon a fresh Windows
profile actually ships — was missing, and with explorer gone there was no other
way to reach it.

- Items now carry a shell *parsing name* (`::{CLSID}` or a filesystem path)
  next to their optional `PathBuf`; label, icon, context menu and opening all
  key off it. The five desktop roots are read from `HideDesktopIcons\NewStartPanel`
  with explorer's own defaults for the values it has never written. Labels come
  from `IShellItem::GetDisplayName`, so they are localized; opening goes through
  `shell:::{CLSID}`, which is not a path and would not survive `ShellExecute`
  otherwise.
- Multi-selection menus now group by "shares one IShellFolder" rather than by
  parent directory, since namespace items have no parent directory.
- **rig: `GUEST-MOUSE.ps1`.** Hyper-V exposes a synthetic keyboard over WMI and
  nothing for the pointer, but the desktop is a mouse surface. The click is made
  inside the guest instead, driven from the console.

Verified in the lab: 휴지통 renders first with the real shell icon and its
Korean label, right-click brings up the genuine shell menu (열기 / 즐겨찾기에
고정 / 휴지통 비우기 grayed because it is empty / 시작 화면에 고정 / 바로 가기
만들기 / 속성), and double-click opens it.

## 2026-07-27

Two more shell-only defects closed, both found by using the guest rather than
reading the code.

- `c0af7b1` **Start search only knew display names.** 명령 프롬프트 parses to
  `{GUID}\cmd.exe`, so `cmd` matched nothing — and with no explorer the Start
  menu is the only launcher the session has. Entries now carry the stem behind
  their parsing name and search falls back to it, display-name hits ranked
  first. Verified: `cmd` finds 명령 프롬프트 and launches it.
- `9c2e09c` **a missed Win release ate every later S.** `WIN_DOWN` is a latch;
  miss one keyup and the hook believes Win is held forever, so every `s` after
  that disappears into the Win+S chord and nothing else misbehaves. Found by
  watching `stop-process` arrive in the guest as `top-proce`. The latch is now
  confirmed against `GetAsyncKeyState`.
- `a44289b` **rig: `keytext`.** After the guest rebooted, `Msvm_Keyboard`
  `TypeText` began swallowing every character while `TypeKey` still landed —
  the rig could press Enter but not write the command it was confirming.
  `keytext` sends a string as virtual keys instead. Shift needs an explicit
  press/key/release with delays; sent back to back the events arrive out of
  order and duplicate the run before them (`Stop-Process` → `Stopstop-process`).

The host slept for ~20 hours mid-session and the guest rebooted on resume,
which accidentally produced the best evidence of the day: a **cold boot with
`e03869a` in place came up with exactly one tray shield**, so the autostart
guard holds through a real Winlogon start, not just a hand restart.

## 2026-07-26 (evening)

**The Start menu fills in, and the tray strip reads as a design.** Three defects
from the morning's first swap are fixed and verified in the guest, and the
safety ladder fired for real — by accident, which is the best way to learn it
works.

- `4a317a2` **the Start menu was never hanging.** Instrumenting the worker
  settled it in one boot: `49 apps from AppsFolder in 349ms`. The reply was
  dropped on delivery, not produced late. `WM_APP_REPLY` finds the menu through
  `GWLP_USERDATA`, and that pointer cannot be installed in `new()` because
  `StartMenu` is returned by value — so it is installed in `show()`, and every
  reply posted before the first open hit a null userdata and was discarded by
  the wndproc prologue. The prewarmed list then sat in the channel with nothing
  left to wake the drain, and `show()` only re-requests when `!loading`, which
  never came back: stuck for the life of the session. `show()` now drains right
  after the pointer lands. The time-boxed Start Menu `.lnk` fallback went in as
  well, since a shell wants it regardless. Verified: 49 apps, icons and Hangul
  section headers, on the first Win-key press after a boot.
- `7182dc6` **one visual language per group in the tray.** Colour app bitmaps,
  our monochrome line glyphs and 한/A as loose text sat in one undifferentiated
  row. The system cells now share a faint capsule with a real gap before the app
  icons, and 한/A got a stroked key cap sized to the glyphs beside it.
- `5d978ca` **desktop labels survive the wallpaper.** A single drop shadow only
  darkens one side; eight one-pixel offsets at low alpha ring the glyphs
  instead. The bamboo wallpaper that made "Microsoft Edge" illegible was the
  test.

**The crash-loop self-destruct works.** Swapping the binary by
`taskkill /f /im glide-shell.exe` counts as a crash — the sentinel is left at
`running` — so three iterations of the edit-build-push loop tripped the ladder:
`Shell=` deleted, explorer respawned, and a dialog explaining both. Exactly the
designed behaviour, reached without meaning to. The lab loop now writes `clean`
to `session.state` and truncates `crash_stamps.txt` as part of the swap.

Rig, for the next session: `taskkill` releases the image lock asynchronously, so
a `move` onto the running exe needs a `timeout /t 5` before it or it fails with
`액세스가 거부되었습니다` and silently restarts the *old* binary — twice mistaken
for a change not landing. After a shell restart the guest has nothing focused
and `Msvm_Keyboard` types into the void; `VM-CONSOLE.ps1 -Action chord`
(`PressKey`/`ReleaseKey`, added in `7182dc6`) sends Alt+Tab, which is the only
way back. Host-side screen capture is unavailable in this session —
`CopyFromScreen` throws `The handle is invalid` — so verification ran off the
1024x768 RGB565 thumbnail, cropped and nearest-neighbour zoomed.

Two new observations: Start search matches display names only, so `cmd` finds
nothing while `명령 프롬프트` would; and every shell start re-runs the Run keys.

- `e03869a` **the duplicate tray icons were ours, not the lab's.** Six identical
  Defender shields looked like an artifact of restarting the shell by hand until
  `tasklist` answered: **seven live `SecurityHealthSystray.exe`**, one per shell
  start. Autostart ran on every start, and Winlogon's AutoRestartShell respins
  us after every crash — so on real hardware one crash loop is enough to
  duplicate every startup app the user has. `run_all` is now keyed to the
  token's AuthenticationId, which is one LUID per logon and survives a restart.
  Verified in the guest: fresh start launches (`autostart: [HKLM Run]
  SecurityHealth — launched`), second start logs `already ran this logon session
  — skipped`, and the process count stays at 1.

  Reaping, checked while chasing this, is fine: killing three systray processes
  dropped exactly three icons — `taskbar.rs` already sweeps owners with
  `IsWindow`.

## 2026-07-26 (morning)

**The swap actually ran.** `Winlogon\Shell` pointed at `glide-shell.exe` in the
lab VM, explorer never started, and Glide came up as the session's only shell —
bar, tray, clock, wallpaper and desktop icons all drawn by us. Three defects
fell out of the first two boots, none of which any run on this box could have
produced.

Getting there took repairing the rig, which had drifted since it was written:
the VM's Guest Service Interface was off, so `Copy-VMFile` failed with
`0x80070015`, and `Enable-VMIntegrationService -Name 'Guest Service Interface'`
does not work on a ko-KR host — the service names come back localized, so it
matches by GUID (`6C09BB55`) now. `lab-cred.xml` was never created, so
PowerShell Direct is still unavailable; everything below was driven through
`VM-CONSOLE.ps1`'s `Msvm_Keyboard` instead, which needs no guest account.

- `5974ff7` **VM-CONSOLE `-Out` with an absolute path.** `Join-Path` concatenates
  a rooted second element rather than replacing the base, so
  `C:\tmp\shot.png` became `C:\repo\C:\tmp\shot.png` and `GetFullPath` threw
  before anything was captured.
- `a62657f` **the console Winlogon allocates.** A console-subsystem binary
  started with no console to inherit gets one, and it sat on top of the desktop
  for the whole session, titled `C:\glint\glide-shell.exe`. Every dev run
  inherits the terminal's console, so this could only appear here. Linking as a
  windows-subsystem binary would take `--register`'s stdin confirmation with
  it, so the console stays and the resident-shell path hides its window;
  hiding rather than freeing keeps the handle valid so the diagnostics still
  write somewhere. Verified: second boot, same swap, no console.
- `ff00bc2` **diagnostics that went nowhere.** Nine one-shot failures reported
  through `eprintln!` — now `safety::note()`. Confirmed by reading `shell.log`
  off the guest screen: `volume OSD subscription failed: HRESULT(0x80070490)`,
  which is a Hyper-V guest having no audio endpoint at all.

**Open, and the reason to keep the lab: the Start menu never fills in.**
`shell:AppsFolder` enumeration does not fail — it does not return. The pane sits
on "앱 목록 불러오는 중..." indefinitely and a search reports 0 matches, and
after the same commit taught `enum_apps` to report all three of its failure
paths *and* a successful-but-empty enumeration, `shell.log` still holds nothing
but the audio line. That rules out a swallowed HRESULT and leaves a hang, which
also explains the icon jobs never arriving: they queue behind it on the one COM
worker. Suspected cause is that the AppsFolder namespace extension wants a
running explorer; the fix is a time-boxed enumeration with the Start Menu
`.lnk` trees as the fallback source, which a shell wants regardless.

Two smaller things seen and not yet chased: with focus on a console window the
Win key reached that window instead of opening our Start menu, so the low-level
hook is not always winning; and Glide's desktop draws the Edge shortcut but not
the Recycle Bin, which is a namespace item rather than a file.

Reading the guest without credentials, for the next session: `Copy-VMFile`
without `-Force` is an existence probe (it fails if the target is there, and a
known-absent control proves the method), and an unelevated
`schtasks /create /sc once /st HH:MM /tr "cmd /k type …\shell.log"` puts a log
on screen where the thumbnail can read it. `/sc onlogon` is refused without
elevation.

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

**Polish pass, from what the screenshots actually showed.**

- `6dde3c2` **ellipsis instead of a hard clip.** Every text cell in the shell is
  fixed width and its content is not, and no format carried a DirectWrite
  trimming sign — so D2D clipped at the layout edge. In Korean that cuts a
  syllable apart: the live captures had `pubg-training 및 1개 ᄃ` on a task
  button, `고급 보안이 포함된 Windows Defer` in the app list, and
  `Microphone Array(디지털 마이크용 인텔® 스마트` in the volume flyout.
  `desktop.rs` already did the right thing for icon labels; that block became
  `render::ellipsize` and now covers the taskbar title format, the Start menu's
  app rows / grid labels / tile labels, the action-center card fields, the toast
  fields, the Task Manager's columns and the settings rows. Character
  granularity, not word — Korean rarely offers a word break near the edge.
  Re-captured the bar, Start menu and volume flyout: all three offenders now end
  in `…`. The notification card fields are the one path still unproven on
  screen — the backlog was empty at capture time.
- `29cfc64` **Task Manager exited nothing.** `WM_CLOSE` called `hide()`, but the
  Task Manager is its own process (`glide-shell --taskmgr`), so closing it left
  a windowless process spinning a message loop with a live sampler thread —
  one orphan per open, each re-enumerating every process on the box every
  1.5 s. Found by counting processes after the verification run, not by reading
  code. Both `WM_CLOSE` and Escape now `DestroyWindow`, and `WM_DESTROY` kills
  the timer and posts the quit. Verified: 1 process before the close, 0 three
  seconds after.

**Planning the M7 lab, which turned out to be a bug hunt.**

The rehearsal needs a machine whose shell can be swapped, crash-looped on
purpose, and reverted in seconds — a VM. Deciding how to build one kept walking
into things that were broken in the product, not in the plan. Four of them, all
in the class of "only fails on a machine we do not control":

- `774249b` **static CRT.** The binaries imported `vcruntime140.dll`. A fresh
  Windows image carries the Universal CRT but not the VC++ redistributable, so
  the shell would have failed to load on the exact machine it is meant to be
  the shell of — and there would have been no shell from which to install the
  redistributable. All three binaries are clean now and the bar still renders.
- `045926a` **WARP fallback.** `D3D11CreateDevice` asked for
  `D3D_DRIVER_TYPE_HARDWARE` and propagated the failure. A Hyper-V guest has no
  hardware device at all; neither does a real box for the seconds its GPU driver
  is being replaced, or an RDP session. Verified by forcing a driver type this
  box cannot provide: the fallback ran, the shell came up on WARP, and every
  surface rendered identically.
- `09d074a` **`safety::note()`.** The fallback above reported itself with
  `eprintln!`, and once Winlogon starts us there is no console behind stderr —
  the one condition worth reporting went nowhere. Now appended to `shell.log`
  beside `crash.log`. Confirmed on disk, UTF-8, with the real `0x887A0004`.
- `f5b745b` **claim `Shell_TrayWnd` whenever we are the system shell.** The
  claim was gated on `--tray-claim` alone, but the swap points `Winlogon\Shell`
  at a plain path. `SHAppBarMessage` is served by whichever window holds that
  class, so with no explorer and no claim there is no server: our own appbar
  reservation is dropped, maximized windows cover the bar, `SPI_GETWORKAREA`
  keeps reporting the full screen, and real apps have nowhere to put a tray
  icon. Three separate mysteries from one cause, found by reading the
  boot-as-shell path while Windows installed rather than by hitting it.

Scripts, all parse-checked: `NEW-LAB-VM.ps1` (Gen 2 + vTPM + Secure Boot for
the Windows 11 requirements, standard checkpoints so a revert keeps running
state, Guest Service Interface for `Copy-VMFile`), `MANAGE-LAB-VM.ps1` (push /
exec / pull / checkpoint / revert), `VM-CONSOLE.ps1` (`Msvm_Keyboard` and
`GetVirtualSystemThumbnailImage`, which read and drive the console with no
cooperation from the guest — the only thing that still answers "what is on
screen" when the shell does not come up).

Reviewing those scripts before running them elevated caught three more, in
`6d66f1f`: integration services were matched by display name on a ko-KR host,
where they come back localized; the vTPM key protector went through
`New-HgsGuardian`, writing certificates into the host store for no benefit;
and `Copy-VMFile` is host-to-guest only, so nothing could come back out.

Edition is Enterprise LTSC on purpose — Shell Launcher, the supported way to
run a custom shell, does not exist on Pro (confirmed on this box: no
`WESL_UserSetting` class, no `Eshell.exe`).

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
