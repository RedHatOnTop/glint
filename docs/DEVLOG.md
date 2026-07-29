# glint devlog

Running development log, newest first. Milestone-level architecture and the swap
plan live in [`SHELL_DESIGN.md`](SHELL_DESIGN.md) §9; this file is the
chronological record of what shipped and what was verified, session by session.

Convention: one dated section per working session. Note the commits, what was
built, and — since these are visual projects — **how it was verified** (a green
build is not evidence that anything rendered).

---

## 2026-07-29

The desktop worked and still looked bought-in: bar and action centre are
floating rounded panels, and a right-click brought up the grey win32 box
`TrackPopupMenuEx` draws. **The context menu is ours now** (`menupopup.rs`).

- The shell still builds a real `HMENU` — `IContextMenu::QueryContextMenu` has
  nowhere else to put its items, and an extension fills its submenu into one —
  but nothing ever shows it. Each level is read back with `GetMenuItemInfoW`
  and painted into a composition window of ours: `WS_EX_NOREDIRECTIONBITMAP`,
  `DWMSBT_TRANSIENTWINDOW`, `DWMWCP_ROUND`, a 0.92-alpha body so the acrylic
  reads through, the accent hairline the overflow panel wears, rounded hover
  rows with an accent pill at the leading edge, and the same rise-and-fade the
  other panels open with.
- A menu loop sends `WM_INITMENUPOPUP` so a shell extension can fill a lazy
  submenu on the way open — 새로 만들기 is empty without it. We have no loop, so
  `shellmenu::init_popup` hands it to the `IContextMenu2/3` by hand just before
  each level is read.
- Levels are their own windows, the root takes capture and focus, and the
  wndproc never creates or destroys a window: it sets `want` / `close_to` and
  the loop applies them after `DispatchMessageW`. Submenus open on an 180ms
  dwell, and only a *change* of row re-arms it.
- `hbmpItem` doubles as an enum — the `HBMMENU_*` marks are tiny integers cast
  to a handle, and asking GDI about one is a crash. Menu bitmaps also arrive
  premultiplied from the shell and straight from older extensions, so the
  pixels get inspected rather than multiplied twice.
- Given up with the menu loop: owner-drawn items. An extension that paints its
  own rows hands out no string, and is skipped rather than drawn as a blank.

Verified in the lab, on the shipping build: background menu and item menu both
render in the panel; 보기 opens on hover with a teal radio bullet and a check
glyph; 새로 만들기 populates with real per-type icons (which is the hand-sent
`WM_INITMENUPOPUP` working); Down/Down/Right/Enter navigates and picks
(`desktop-view.txt` → `sort=name`); a *mouse* click on 바로 가기 만들기 in
drag-me.txt's menu invoked the shell verb and `drag-me - 바로 가기` appeared on
the desktop; a click on bare desktop dismisses, Esc dismisses; the rounded
corner and the hairline sampled (41,81,82) against a (24,32,33) body.

Not a regression, established by A/B: New ▸ 텍스트 문서 and New ▸ 폴더 populate
and paint but create nothing. Stashing this slice and running the previous
`TrackPopupMenuEx` binary in the same lab behaves identically — `NewMenu` wants
a shell-view site we do not give it. 액세스 권한 부여 ▸ is empty in both, too.

**The icons were an explorer copy too.** White label ringed in a black halo,
translucent-accent wash over the selected cell — that is explorer's look, drawn
by us. Both are gone.

- A label now sits on a rounded chip cut from the same surface as the bar and
  the menus. `IDWriteTextLayout` per item, measured once and cached, so the
  chip fits the text instead of the cell — and it is one text draw where the
  halo was nine.
- A selected cell is a card in the panel colour with the accent hairline along
  its top edge, and its chip goes accent with dark ink. Bright accents make
  white text on them the unreadable combination, not the safe one.
- Hover is the shared `HOVER_FILL` wash on the same card, and the marquee is
  rounded to match.
- Cell padding went 50 → 54: the chip's own padding has to fit under two lines
  of label, and at 50 every wrapped name came back trimmed to one line and an
  ellipsis (caught in the lab, not in review).

Verified in the lab on the running shell: 휴지통 / drag-me.txt / Microsoft Edge
/ drag-me - 바로 가기 all render with two-line names on chips; hover measured as
a +3.8% brightness lift over the cell (264.8 → 293.6 mean RGB sum, thumbnails
`ic6`/`ic7`); a click puts the card, the accent hairline and the teal chip up.

Rig note: `GUEST-MOUSE.ps1` injects into *whatever session runs it*. It must be
typed into the guest console (`powershell -ep bypass -f C:\glint\GUEST-MOUSE.ps1
…`); running it from the host clicks the host, and four "the menu will not
dismiss" rounds were nothing but clicks that never reached the VM.

**Third weakness: the desktop looked finished and did not act it.** No
clipboard, no F5, arrows moved the cursor but could not extend a selection, and
typing a letter did nothing. All four are in.

- Ctrl+C / Ctrl+X / Ctrl+V. The data object is not ours: the selection's PIDLs
  go into `SHCreateShellItemArrayFromIDLists`, and `BindToHandler(BHID_DataObject)`
  hands back the shell's own `IDataObject` — CF_HDROP and every shell format
  with it. Cut versus copy rides in the registered `Preferred DropEffect`
  format, one DWORD in an HGLOBAL, which is what explorer reads too.
- Paste is a simulated drop onto the Desktop folder's own `IDropTarget`
  (`BHID_SFUIObject`): `DragEnter` / `DragOver` / `Drop`. **`grfKeyState` must
  carry `MK_LBUTTON`.** Without a mouse button in it the shell target reads the
  drop as a *right*-drag and answers with the 여기에 복사 / 취소 menu instead of
  pasting — an hour lost to that.
- Cut items are drawn at 0.45 alpha, icon and chip together, and Esc clears the
  ghosting the way it clears everything else.
- Shift+arrow extends: the cursor keeps an anchor, and the selection is the
  rectangle of cells between anchor and cursor, not the linear run — the icons
  are on a grid and a grid selection is what the eye expects.
- A letter key selects the next item whose name starts with it; the same letter
  again cycles through the matches. The prefix accumulates for 1.2s, so `de`
  reaches `desktop-view.txt` past `drag-me.txt`.
- F5 re-enumerates.

Verified in the lab on the running shell, driven from the guest console:
Shift+Down puts two cards and two accent chips up (`kb2`); Ctrl+C then Ctrl+V
produced `drag-me - 복사본.txt` on the desktop with no menu in the way (`kb4`);
Ctrl+X ghosted it (`kb5`); Esc then `m` jumped the selection to Microsoft Edge
(`kb6`); F5 came back with the desktop whole and the new copy still on it
(`kb7`).

**And the desktop is a drag source now.** It accepted drops and could not give
one: a file could come in from anywhere and could not leave except through the
clipboard.

- `IDropSource` is the one interface the shell cannot supply — it is the
  *source's* judgement of when the drag ends — but both methods are pure
  policy and both get the standard answer: cancel on Escape or the right
  button, drop when the button that started it comes up, default cursors.
  The data object is the same `selection_data()` the clipboard uses.
- The hand-off happens in `WM_MOUSEMOVE`: past the system drag threshold and
  over a window that is not one of ours, the drag stops being a rearrangement
  and becomes an export — icons snap back, capture is released, `DoDragDrop`
  takes the pointer. `WindowFromPoint` is not confused by our own capture, and
  the secondary desktops count as ours, so crossing a monitor edge stays a
  move within one folder.
- `DoDragDrop` pumps this wndproc reentrantly, so no `Desktop` borrow lives
  across it — same discipline as the context menu.

Verified in the lab: dragging `drag-me - 복사본.txt` from the desktop onto a
Notepad window opened it there — title `drag-me - 복사본.txt`, body `dragged`
(`ds3`) — and the icon stayed where it was. Dragging within the desktop still
rearranges: `drag-me.txt` moved a column right and a row down and stayed there
(`ds6`).

**Dropping onto an icon**, the last of the four. An internal drag only ever
rearranged, so 휴지통 was a picture and a folder icon was a place to put another
icon next to. Letting go over an icon now asks that item whether it takes drops
(`SFGAO_DROPTARGET`) and, if it does, hands it the selection through its own
`IDropTarget` with no modifier — the target picks the effect, which is what
makes a drag to the bin a delete and a drag to a folder a move. An icon that is
part of the selection is not a place to drop the selection, and anything that
says no falls through to the rearrangement it always was. The three drops we do
now — paste, onto-icon, and the desktop's own — share one `simulate_drop`.

Verified in the lab: dragging `drag-me - 복사본.txt` onto 휴지통 took it off the
desktop and the bin's own listing shows it (`Shell.Application` NameSpace(10) →
`drag-me - 복사본`), so it was recycled and not erased; dragging `drag-me.txt`
onto an `inbox` folder icon emptied its cell, filled the folder glyph, and
`dir /b Desktop\inbox` answers `drag-me.txt`.

Rig note, the second: a test script that minimizes the console must put it back.
`act.ps1` does not, and the next `keytext` went to the *desktop* instead —
where the new type-ahead read it and Enter opened a folder. Nothing was broken,
but a whole round was spent reading a screen that showed the wrong thing (it did
prove type-ahead and Enter work against a real key stream).

**새로 만들기 is ours and it creates files now** (`newmenu.rs`). It had been the
one item on the desktop menu that painted perfectly and did nothing — the
shell's `NewMenu` fills the submenu on `WM_INITMENUPOPUP`, icons and all, and
then wants a shell-view site to select and rename what it made, which a
composition window is not.

- The entries come from the registry `NewMenu` itself reads, the file is written
  here, and the caller drops the new icon straight into inline rename the way
  explorer does. `Data` before `NullFile`, because `.zip` carries both and an
  empty `.zip` is a broken file.
- Every word comes out of shell32's string table (`SHLoadIndirectString`), so a
  Korean install says 폴더 / 새 폴더 and an English one says Folder / New folder
  with no translation of ours.
- Nothing is painted that cannot be delivered: a `FileName` template whose file
  is not on disk, a `Command` whose program is not installed, and any entry with
  a `Handler` (a COM class wanting that same shell-view site) are dropped while
  the menu is built rather than shown and then fumbled. 바로 가기 goes with
  them — its ShellNew names a Handler, and the classic stand-in `rundll32
  appwiz.cpl,NewLinkHere` is inert on this Windows from the command line too.
- 붙여넣기 is on the background menu again, enabled off `IsClipboardFormatAvailable`.
  The shell's own background menu has no paste item, so there was nothing to
  filter through — it had to be one of ours.

Three registry facts cost a round each, all found by reading the live registry
rather than guessing:

- `ItemName` is the *file's* name, not the menu label — the menu read 새 비트맵
  이미지 until it moved to where it belongs, and it is now what the created file
  is called, so we land on explorer's exact `새 압축(ZIP) 폴더.zip`.
- `.zip` keeps its ShellNew under a ProgID subkey — `HKCR\.zip\CompressedFolder\ShellNew`
  — not under the extension. A menu that reads only the extension is missing
  압축(ZIP) 폴더 and cannot tell you why.
- Half of these values are REG_EXPAND_SZ and arrive with `%SystemRoot%` intact,
  and a `Command` is `"…\Wab.exe" /CreateContact "%1"` — quoted, so splitting at
  the first space hands `ShellExecuteW` a path that ends mid-word.

The verb filter that was supposed to keep the shell's own duplicates out had
been doing nothing: `GetMenuItemID` answers -1 for an item that owns a submenu,
which is exactly what 새로 만들기 and 액세스 권한 부여 are. `GetMenuItemInfoW`
with `MIIM_ID` gives the real id, and both are gone from the merged menu.

Verified in the lab on the shipping build: the submenu reads 폴더 / 비트맵 이미지
/ 압축(ZIP) 폴더 / 연락처 / 텍스트 문서 (`nm43`); 비트맵 이미지 put
`새 비트맵 이미지.bmp` on the desktop with the stem selected in inline rename
(`nm29`, `nm31`); 압축(ZIP) 폴더 produced `새 압축(ZIP) 폴더.zip`, 22 bytes, which
`ZipFile.OpenRead` opens (`nm42`, `nm44`); 연락처 opened wab.exe's contact sheet
(`nm45`) — an entry explorer hides and ours actually runs.

Rig note, the third: the lab swap script raced Winlogon. Killing the shell to
overwrite its exe gives Winlogon its cue to start it again, so the copy lands on
a locked file, says nothing, and the next round is spent testing the build you
thought you had replaced (a timestamp on `C:\glint\*.exe` is what caught it).
Retrying the kill lost too (six times, two seconds apart); what wins is that a
running image cannot be deleted but *can* be renamed, so `swap.cmd` moves the
old exe aside first and the copy lands on nothing. Winlogon — not `start` —
brings the new image up. And the whole menu scenario has to fit
inside one `act.ps1 -Hold`: when it expires the console comes back to the
foreground and takes the menu down with it, so keystrokes sent after a host-side
round trip land in the console instead.

**The item menu killed the shell**, and the verb filter above is what did it.
A right-click on a desktop icon left a black screen with no `crash.log`, so not
a panic — the process was simply gone. Item menus call the same `run()` with an
*empty* kill list, and the filter had no early-out, so `GetCommandString` was
asked for the verb of every row the shell extensions had just built, submenu
owners included. The old `GetMenuItemID == u32::MAX` check had been skipping
those by accident; reading the id properly took the accident away. The filter
now returns before it asks anything when there is nothing to match against, and
the item menu is back.

That left rename with nowhere to live — the shell only offers 이름 바꾸기 when
asked with `CMF_CANRENAME`, and invoking it wants a shell view site we do not
have. So the flag goes in, the shell places the item where explorer places it
(between 삭제 and 속성, not first, which is where a prepended custom entry would
have landed), and `run()` reads the verb of the *one* row the user picked —
after the menu is down, one call, not one per row — and returns
`MenuOutcome::Rename` for our own inline editor instead of invoking it.

Two more explorer gestures, both keyboard-and-wheel: **Shift+F10 and the context
key** raise the menu at the focused icon (or at the grid origin when nothing is
selected), and **Ctrl+wheel** steps the icon size. `WM_CONTEXTMENU` never
arrives at this window — it is `WS_EX_NOACTIVATE` and `DefWindowProc` does not
synthesize one — and `F10` comes in as `WM_SYSKEYDOWN` even with Shift held, so
both are handled as keys.

Verified in the lab on the shipping build: right-click on 새 폴더 draws the full
item menu with the shell alive after it (`h09`); 이름 바꾸기 opens the inline
editor with the stem selected (`h14`); Shift+F10 draws the same menu at the
selected icon (`h16`); the context key on empty ground draws the background menu
(`h18`); Ctrl+wheel up twice grows the icons and three notches down shrink them
past the size they started at (`h19`, `h23`).

**The Win chords were all dead.** Win+E, Win+R, Win+D — explorer answered those,
and with explorer gone nobody does: the chord reaches no window and the bare
letter falls through to whatever has focus (`erd` accumulated in the console
while I probed them). The Win hook was already there for the bare press and
Win+S, so claiming the rest is the same swallow-and-mask path, generalized: a
`CLAIMED` table, one `SWALLOW_VK` latch instead of the S-only flag, and a
`WM_WINKEY_COMBO` carrying the VK to the bar.

- **E** glide, **R** shell32's run dialog, **D** show-desktop toggle (the bar
  already had one), **M** minimize all, **I** our settings app, **A** action
  centre, **1**-**9** the bar's slots left to right, **X** a power-user menu of
  our own — file manager / run / task manager / settings / sign out / restart /
  shut down, drawn with `menupopup` above the start button.
- Win+R goes through `rundll32 shell32.dll,#61` rather than calling `RunFileDlg`
  in-process: the dialog is modal and would freeze the bar for as long as it is
  open. The cost is that its description reads "RunDLL" — a run box of our own
  is the fix, and it is not written yet.
- Win+L stays the system's (winlogon), Win+Shift+S stays the snipper: a
  modifier other than Win means the chord was never ours.

Verified in the lab, each by the thing it does: Win+R draws the run dialog
(`k14`), Win+A the action centre (`k15`), Win+X our menu above the start button
(`k18`), Win+D minimizes everything and a second press brings the same set back
(`k16`, `k17`), Win+I opens our settings app (`k18`), Win+1 minimizes the
console and Win+1 again restores it (`k24`, `k25`). Win+E swallowed its key —
the hook has it — but glide never draws in this VM: it exits 0 with no window,
GPU-less, and eframe wants a GL surface. That half is unverified here and will
have to be checked on real hardware.

Rig note, the fourth — two rig bugs faked two results. `act.ps1 -A` took a
`[string[]]`, and `-File` bound only the first of `R62,590 L140,531` because each
token already contains a comma; the second action was dropped in silence and the
menu looked like it had ignored the click. Actions are one `;`-separated string
now. And in `GUEST-MOUSE.ps1` the flag constant `$WHEEL = 0x0800` *was* the
`-Wheel` parameter — PowerShell variable names are case-insensitive — so every
scroll went `120 * 2048` notches up regardless of direction, and Ctrl+wheel-down
read as "the shell only grows icons". Renamed `$MWHEEL`.

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

Desktop gap 2 of 4: **the desktop only noticed changes when something else
poked it.** A file saved to the desktop stayed invisible until a
WM_SETTINGCHANGE happened by.

- `SHChangeNotifyRegister` on the two desktop folders and the five namespace
  roots, with a 250ms debounce timer because one user action arrives as a burst
  of notifications. Explorer's mechanism rather than `ReadDirectoryChangesW`,
  because half of what the desktop shows is not a directory — a file going into
  the recycle bin changes that icon and no filesystem event says so.
- The registration goes in after `GWLP_USERDATA` is installed, since a delivery
  that lands on a null userdata is a leaked shared-memory handle.

Verified in the lab, both halves: `echo watch> %USERPROFILE%\Desktop\watch-test.txt`
from the console and the file appeared with no refresh; recycling it through
`Shell.Application.InvokeVerb('delete')` — a path that touches none of our own
code — removed the icon *and* switched 휴지통 from the empty bin to the full one.

Desktop gap 3 of 4: **the desktop was mouse-only.** It could not even receive a
keystroke — `WS_EX_NOACTIVATE` plus `MA_NOACTIVATE` meant it never held focus.

- Dropped both. The window is activatable now, and `WM_WINDOWPOSCHANGING` still
  pins it to the bottom of the z-order, which is precisely what explorer's
  desktop is: focusable and behind everything. `WS_EX_TOOLWINDOW` keeps it out
  of Alt+Tab.
- Arrows (the grid is column-major, and running off a column carries into the
  next), Ctrl+A, Enter, Esc, Delete (`SHFileOperation` with `FOF_ALLOWUNDO`,
  shift to erase, one call for the batch so there is one undo entry), F2.
- F2 opens a real EDIT window rather than something drawn here: a self-drawn box
  would have to reimplement IME composition and these are Korean filenames. It
  is a popup and not a child, because the desktop is `WS_EX_NOREDIRECTIONBITMAP`
  and a child HWND has no surface to compose into — it would simply not appear.
  The stem is preselected and the extension is not, as explorer does.

Verified in the lab, every one: Ctrl+A framed all three icons, ↓ narrowed to
one, F2 opened the box with `kb-test` selected and `.txt` not, typing +Enter
renamed the file and the list re-sorted itself through the watcher, Delete
removed it, ↑ then Enter opened 휴지통 — which listed `renamed` and `watch-test`
with 원래 위치 `C:\Users\Person\Desktop`, so Delete recycles rather than erases.

Desktop gap 4 of 4: **nothing could be dropped on the desktop.** The window was
never registered as a target, so a drag from any explorer window bounced.

- `RegisterDragDrop` with the Desktop folder's *own* `IDropTarget`, obtained
  through `BindToHandler(BHID_SFUIObject)`. Not a hand-written implementation:
  the shell's already knows copy against move against link, what the modifier
  keys mean, what to do with a `.lnk` and what to do with a dragged URL. Ours
  would only be a worse version of it. `OleInitialize` first — the thread had
  only been `CoInitializeEx`'d, and `RegisterDragDrop` wants OLE.
- **rig: `GUEST-MOUSE.ps1 -Click drag`.** `DoDragDrop` runs a modal loop on the
  source thread reading the real cursor, so the move has to arrive as ~25 small
  steps with time between them; one jump lands as a click on the source and
  nothing is ever dragged.

Verified in the lab: `drag-me.txt` dragged out of a `C:\dragsrc` explorer window
onto bare desktop. The folder went to 0개 항목 and the file appeared in the icon
column — a same-volume drag, so a move, which is what explorer would have done.

That closes the four desktop gaps.

**Icons drag, and stay where they are put.** Until now the layout was derived
from the item order every time, so there was nowhere to *put* an icon.

- The grid cell is the authority and x/y follow from it. Items take the cell
  they were dragged to, and everything else fills the first free cell in
  reading order, so a gap the user left stays a gap.
- Dragging moves the whole selection by one delta, snapped, with anything
  landing on an occupied cell sliding to the next free one instead of stacking.
  The threshold is `SM_CXDRAG`, so a click with a shaky hand is still a click,
  and a lost capture abandons the drag rather than dropping icons somewhere
  arbitrary.
- Positions live in `%APPDATA%\glide-shell\desktop-icons.txt` as
  `col,row=parsing name` — explorer's own store is an undocumented ItemPos blob.
- Arrow keys had to move to the cells too; after a drag, item order and screen
  position have nothing to do with each other. They now step cell by cell and
  skip the empty ones.

Verified in the lab: 휴지통 dragged from (0,0) to mid-screen, snapped to the
grid, and the file read back `4,3=::{645FF040-…}` with the other two still at
`0,1` and `0,2`. After a full shell restart it came up in the same place, gap at
(0,0) intact.

**One desktop per monitor.** There was exactly one window, sized from
`SM_CXSCREEN`/`SM_CYSCREEN` at (0,0), so every monitor but the primary got
nothing at all: no wallpaper, no background menu, not even a surface to click.

- `EnumDisplayMonitors` at startup, one window per monitor, each with its own
  renderer at its own monitor's effective DPI. Icons stay on the primary, which
  is where explorer keeps them; the others carry wallpaper and the background
  menu. A monitor is identified by its device name — HMONITOR handles do not
  survive a display change, so nothing can be matched back by handle.
- Icon coordinates became window-relative and the work area is now read per
  monitor (`GetMonitorInfoW`, not `SPI_GETWORKAREA`, which only ever knew the
  primary). Cell (0,0) follows the work area's own left edge as well as its top.
- Wallpaper comes from `IDesktopWallpaper` matched by monitor rect, since
  Windows holds a separate image per monitor and `SPI_GETDESKWALLPAPER` reports
  one of them; SPI stays as the fallback for a spanned image.
- `WM_DISPLAYCHANGE` schedules one debounced rescan: monitors that stayed are
  moved and rescaled, a new one gets a window, and a departed one has its window
  closed. Closing goes through `WM_CLOSE` rather than `DestroyWindow` because
  the rescan runs off a message and the window being retired can be the one
  whose wndproc is on the stack. `WM_DPICHANGED` refits the same way.
- **rig: `GUEST-DISPLAY.ps1`**, and `GLIDE_DESK_SPLIT` alongside the existing
  `GLIDE_DESK_OFF`/`GLIDE_DESK_BARE` — the lab VM has one screen, and the
  two-window path is the whole change, so the seam splits that screen down the
  middle into two pseudo-monitors.

Verified in the lab, three ways. Single monitor first: no regression, all three
icons still in their saved cells with the wallpaper intact. Then `GLIDE_DESK_SPLIT=1`:
two windows, each cover-cropping the wallpaper to its own 512×768 half (the seam
at x=512 is unmistakable), icons on the left half only, and a right-click on the
right half bringing up the full shell background menu — so the secondary is a
live window, not a painted bitmap. Then the display-change path on a live shell:
`ChangeDisplaySettings` to 800×600 and back to 1024×768, with the desktop
refitting both ways — window resized, wallpaper re-cropped for the new size,
icons still on their cells.

Not verified: a real second monitor (this lab VM has one screen — Hyper-V only
does multi-monitor over enhanced-session RDP), and therefore neither genuinely
different per-monitor wallpapers nor a window being created or closed as a
monitor arrives or leaves.

Also fixed in the rig: `VM-CONSOLE.ps1 -Action keytext` typed every capital as
lowercase. PowerShell hashtable keys are case-insensitive, so `Y` found the `y`
entry and never took the shift branch — which is how a `YES` confirmation prompt
came back as `yes` and cancelled a shell registration.

**보기 and 정렬 기준.** The background menu had four flat entries and no view
options at all: one icon size, one order, no way to say either.

- `CustomItem` grew from an `(id, label, enabled)` tuple into a struct with
  children, a checked flag and a radio flag, so our half of the menu can nest
  and show state. Items go in with `InsertMenuItemW` now rather than
  `AppendMenuW` — `MFT_RADIOCHECK` is per-item there, which is what makes an
  exclusive group read as bullets instead of ticks.
- 보기: 큰/보통/작은 아이콘 (96/48/32), 아이콘 자동 정렬, 바탕 화면 아이콘 표시.
  정렬 기준: 이름 / 크기 / 항목 유형 / 수정한 날짜, folders always leading and
  the name breaking every tie so a layout is stable between reads.
- The cell is `icon + padding` now instead of two constants, so one number
  drives layout, hit testing, the marquee and the rename box.
- Sorting, or turning auto-arrange on, repacks and overwrites saved positions —
  what explorer does. Changing the icon size does not: the grid changes shape
  but the cell an icon was dragged to still means the same thing.
- No 아이콘을 그리드에 맞춤 entry: this layout is grid-snapped always, and a
  toggle that cannot be off would be a lie.
- Settings live in `%APPDATA%\glide-shell\desktop-view.txt`, beside the
  positions.

Verified in the lab, each one: the menu opens with 보기 ▸ and 정렬 기준 ▸ above
the shell's own items, the current size carries a radio bullet and 바탕 화면
아이콘 표시 a tick; 큰 아이콘 scaled the icons to 96px with all three staying in
their own cells; 수정한 날짜 repacked into newest-first and wrote the new cells
to disk; 아이콘 자동 정렬 made a drag to mid-screen snap back to (0,0); 바탕 화면
아이콘 표시 cleared the desktop to bare wallpaper and restored it. The file read
back `icon=96 / sort=modified / auto_arrange=0 / show_icons=1`.

That closes the desktop list: namespace items, folder watch, keyboard, drop
target, drag with saved positions, multi-monitor, view options.

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
