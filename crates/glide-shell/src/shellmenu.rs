//! Native shell context menu host for the desktop window — glide's
//! shellmenu.rs adapted: no verb reimplementation here, so item menus keep
//! every shell verb; multi-selection maps to one GetUIObjectOf over the
//! child pidls (single parent folder). TrackPopupMenuEx blocks and pumps on
//! the UI thread; "새로 만들기"-style submenus populate via IContextMenu2/3
//! message forwarding through a temporary subclass.

use std::cell::RefCell;
use std::path::Path;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, GCS_VERBW, IContextMenu, IContextMenu2,
    IContextMenu3, IShellFolder, SHBindToParent, SHGetDesktopFolder, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DeleteMenu, DestroyMenu, GetCursorPos, GetMenuItemCount,
    GetMenuItemInfoW, HMENU, InsertMenuItemW, MENU_ITEM_STATE, MENU_ITEM_TYPE,
    MENUITEMINFOW, MF_BYPOSITION, MF_SEPARATOR, MFS_CHECKED, MFS_GRAYED, MFT_RADIOCHECK,
    MFT_SEPARATOR, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, PostMessageW,
    WM_INITMENUPOPUP, WM_NULL,
};
use windows::core::{Interface, PCSTR, PCWSTR, PSTR, PWSTR, w};

pub enum MenuOutcome {
    /// One of the caller's prepended items was picked (its id).
    Custom(u32),
    /// A shell verb ran — caller should refresh the listing.
    Invoked,
    Dismissed,
}

/// Caller-supplied menu entry, prepended above the shell's own items. An empty
/// label is a separator; children make it a submenu and its id goes unused.
pub struct CustomItem {
    pub id: u32,
    pub label: String,
    pub enabled: bool,
    pub checked: bool,
    /// Show the check as a radio bullet — one of a set rather than a toggle.
    pub radio: bool,
    pub children: Vec<CustomItem>,
}

impl CustomItem {
    pub fn new(id: u32, label: &str) -> Self {
        Self {
            id,
            label: label.into(),
            enabled: true,
            checked: false,
            radio: false,
            children: Vec::new(),
        }
    }

    pub fn sep() -> Self {
        Self::new(0, "")
    }

    pub fn submenu(label: &str, children: Vec<CustomItem>) -> Self {
        Self { children, ..Self::new(0, label) }
    }

    pub fn check(id: u32, label: &str, on: bool) -> Self {
        Self { checked: on, ..Self::new(id, label) }
    }

    pub fn radio(id: u32, label: &str, on: bool) -> Self {
        Self { checked: on, radio: true, ..Self::new(id, label) }
    }
}

const ID_SHELL_FIRST: u32 = 0x1000;
const ID_SHELL_LAST: u32 = 0x7FFF;

// Background menu only. 새로 고침: the shell verb repaints nothing in our
// window, and the caller has its own. 새로 만들기: NewMenu paints entries it
// cannot then create anything from without a shell-view site, so newmenu.rs
// replaces it. 액세스 권한 부여: comes up empty for the same reason.
const BG_VERB_FILTER: &[&str] = &["refresh", "new", "windows.share"];

thread_local! {
    static MENU_FWD: RefCell<Option<(Option<IContextMenu2>, Option<IContextMenu3>)>> =
        const { RefCell::new(None) };
}

/// Opt this process's win32 menus into the dark theme. Undocumented uxtheme
/// ordinals (135 = SetPreferredAppMode, 136 = FlushMenuThemes) — guarded, a
/// miss just leaves the menus light.
pub fn enable_dark_menus() {
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else {
            return;
        };
        type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
        type FlushMenuThemes = unsafe extern "system" fn();
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(135 as *const u8)) {
            let set_mode: SetPreferredAppMode = std::mem::transmute(f);
            set_mode(1); // AllowDark — follows the system setting
        }
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(136 as *const u8)) {
            let flush: FlushMenuThemes = std::mem::transmute(f);
            flush();
        }
    }
}

/// A menu loop sends WM_INITMENUPOPUP so a shell extension can fill its
/// submenu on the way open — 새로 만들기 is empty without it. We draw the menu
/// ourselves and have no loop, so menupopup calls this instead, just before it
/// reads a level's items.
fn init_popup(hmenu: HMENU, pos: u32) {
    MENU_FWD.with(|f| {
        let f = f.borrow();
        let Some((cm2, cm3)) = f.as_ref() else { return };
        let wparam = WPARAM(hmenu.0 as usize);
        let lparam = LPARAM(pos as isize);
        unsafe {
            if let Some(cm3) = cm3 {
                let mut lres = LRESULT(0);
                if cm3
                    .HandleMenuMsg2(WM_INITMENUPOPUP, wparam, lparam, Some(&mut lres))
                    .is_ok()
                {
                    return;
                }
            }
            if let Some(cm2) = cm2 {
                let _ = cm2.HandleMenuMsg(WM_INITMENUPOPUP, wparam, lparam);
            }
        }
    });
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// IContextMenu over one or more items, named the way the shell names them:
/// a filesystem path, or `::{CLSID}` for a namespace item like the recycle
/// bin. All must share a parent — the folder object comes from the first.
unsafe fn item_menu(paths: &[String]) -> windows::core::Result<IContextMenu> {
    unsafe {
        let mut full: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
        let mut children: Vec<*const ITEMIDLIST> = Vec::with_capacity(paths.len());
        let mut folder: Option<IShellFolder> = None;
        let mut err = None;
        for path in paths {
            let w = wide(path);
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if let Err(e) = SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None) {
                err = Some(e);
                break;
            }
            full.push(pidl);
            let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
            match SHBindToParent::<IShellFolder>(pidl, Some(&mut child)) {
                Ok(f) => {
                    if folder.is_none() {
                        folder = Some(f);
                    }
                    children.push(child as *const ITEMIDLIST);
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let out = match (err, folder) {
            (None, Some(folder)) => {
                folder.GetUIObjectOf::<IContextMenu>(HWND::default(), &children, None)
            }
            (Some(e), _) => Err(e),
            _ => Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL)),
        };
        for pidl in full {
            CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
        }
        out
    }
}

/// Background IContextMenu of a directory (새로 만들기/붙여넣기/extensions).
unsafe fn background_menu(dir: &Path) -> windows::core::Result<IContextMenu> {
    let w = wide(&dir.display().to_string());
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe {
        SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None)?;
        let desktop = SHGetDesktopFolder()?;
        let out = desktop
            .BindToObject::<Option<&IBindCtx>, IShellFolder>(pidl, None)
            .and_then(|folder| folder.CreateViewObject::<IContextMenu>(HWND::default()));
        CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
        out
    }
}

/// Delete shell entries whose canonical verb duplicates one of ours, then
/// collapse the separator runs the deletions leave behind.
unsafe fn filter_verbs(
    hmenu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    cm: &IContextMenu,
    kill: &[&str],
) {
    unsafe {
        let count = GetMenuItemCount(Some(hmenu));
        for i in (0..count).rev() {
            // Not GetMenuItemID: it answers -1 for an item that owns a
            // submenu, which is exactly what 새로 만들기 and 액세스 권한 부여
            // are, so they slipped through every filter until now.
            let mut mii = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID,
                ..Default::default()
            };
            if GetMenuItemInfoW(hmenu, i as u32, true, &mut mii).is_err() {
                continue;
            }
            let id = mii.wID;
            if id < ID_SHELL_FIRST {
                continue;
            }
            let mut buf = [0u16; 128];
            if cm
                .GetCommandString(
                    (id - ID_SHELL_FIRST) as usize,
                    GCS_VERBW,
                    None,
                    PSTR(buf.as_mut_ptr() as *mut u8),
                    buf.len() as u32,
                )
                .is_ok()
            {
                let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
                let verb = String::from_utf16_lossy(&buf[..len]).to_lowercase();
                if kill.contains(&verb.as_str()) {
                    let _ = DeleteMenu(hmenu, i as u32, MF_BYPOSITION);
                }
            }
        }
        let mut prev_sep = true; // also strips a leading separator
        let mut i = 0;
        while i < GetMenuItemCount(Some(hmenu)) {
            let mut mii = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE,
                ..Default::default()
            };
            let is_sep = GetMenuItemInfoW(hmenu, i as u32, true, &mut mii).is_ok()
                && (mii.fType & MFT_SEPARATOR) == MFT_SEPARATOR;
            if is_sep && prev_sep {
                let _ = DeleteMenu(hmenu, i as u32, MF_BYPOSITION);
                continue; // same index now holds the next item
            }
            prev_sep = is_sep;
            i += 1;
        }
        let n = GetMenuItemCount(Some(hmenu));
        if n > 0 {
            let mut mii = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE,
                ..Default::default()
            };
            if GetMenuItemInfoW(hmenu, (n - 1) as u32, true, &mut mii).is_ok()
                && (mii.fType & MFT_SEPARATOR) == MFT_SEPARATOR
            {
                let _ = DeleteMenu(hmenu, (n - 1) as u32, MF_BYPOSITION);
            }
        }
    }
}

/// Our own entries, in order, submenus and all. The shell's items are queried
/// in after these, from `ID_SHELL_FIRST` up, so the two id spaces never meet.
unsafe fn append_custom(hmenu: HMENU, items: &[CustomItem]) {
    unsafe {
        for it in items {
            if it.label.is_empty() {
                let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, None);
                continue;
            }
            let mut label = wide(&it.label);
            let mut state = MENU_ITEM_STATE(0);
            if !it.enabled {
                state |= MFS_GRAYED;
            }
            if it.checked {
                state |= MFS_CHECKED;
            }
            let mut info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID | MIIM_STRING | MIIM_STATE | MIIM_FTYPE,
                fType: if it.radio { MFT_RADIOCHECK } else { MENU_ITEM_TYPE(0) },
                fState: state,
                wID: it.id,
                dwTypeData: PWSTR(label.as_mut_ptr()),
                ..Default::default()
            };
            if !it.children.is_empty()
                && let Ok(sub) = CreatePopupMenu()
            {
                append_custom(sub, &it.children);
                info.fMask |= MIIM_SUBMENU;
                info.hSubMenu = sub;
            }
            let at = GetMenuItemCount(Some(hmenu)) as u32;
            let _ = InsertMenuItemW(hmenu, at, true, &info);
        }
    }
}

unsafe fn run(
    hwnd: HWND,
    cm: IContextMenu,
    custom: &[CustomItem],
    verb_filter: &[&str],
) -> MenuOutcome {
    unsafe {
        let Ok(hmenu) = CreatePopupMenu() else {
            return MenuOutcome::Dismissed;
        };
        append_custom(hmenu, custom);
        if !custom.is_empty() {
            let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, None);
        }
        let insert_at = GetMenuItemCount(Some(hmenu)) as u32;
        if cm
            .QueryContextMenu(hmenu, insert_at, ID_SHELL_FIRST, ID_SHELL_LAST, 0)
            .is_err()
        {
            let _ = DestroyMenu(hmenu);
            return MenuOutcome::Dismissed;
        }
        filter_verbs(hmenu, &cm, verb_filter);

        MENU_FWD.with(|f| *f.borrow_mut() = Some((cm.cast().ok(), cm.cast().ok())));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let cmd = crate::menupopup::track(hwnd, hmenu, pt.x, pt.y, init_popup);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        MENU_FWD.with(|f| *f.borrow_mut() = None);
        let _ = DestroyMenu(hmenu);

        if cmd == 0 {
            MenuOutcome::Dismissed
        } else if cmd < ID_SHELL_FIRST {
            MenuOutcome::Custom(cmd)
        } else {
            let offset = cmd - ID_SHELL_FIRST;
            let info = CMINVOKECOMMANDINFOEX {
                cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
                fMask: 0x0000_4000, // CMIC_MASK_UNICODE
                hwnd,
                lpVerb: PCSTR(offset as usize as *const u8),
                lpVerbW: PCWSTR(offset as usize as *const u16),
                nShow: 1, // SW_SHOWNORMAL
                ..Default::default()
            };
            let _ = cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO);
            MenuOutcome::Invoked
        }
    }
}

/// Full shell menu for `paths` — shell parsing names sharing one parent. COM
/// is already up on the taskbar thread.
pub fn show_item_menu(hwnd: HWND, paths: &[String]) -> MenuOutcome {
    unsafe {
        match item_menu(paths) {
            Ok(cm) => run(hwnd, cm, &[], &[]),
            Err(_) => MenuOutcome::Dismissed,
        }
    }
}

pub fn show_background_menu(hwnd: HWND, dir: &Path, custom: &[CustomItem]) -> MenuOutcome {
    unsafe {
        match background_menu(dir) {
            Ok(cm) => run(hwnd, cm, custom, BG_VERB_FILTER),
            Err(_) => MenuOutcome::Dismissed,
        }
    }
}
