// Native shell context menu (IContextMenu) — shows everything Explorer shows,
// including installed shell extensions (Open with ..., 터미널에서 열기, Git ...).
// Our own items are prepended as regular HMENU entries; ids below ID_SHELL_FIRST
// come back as MenuOutcome::Custom. Must run on the UI thread: TrackPopupMenuEx
// blocks and pumps, and "새로 만들기"-style submenus need menu messages forwarded
// to IContextMenu2/3 via a temporary window subclass.
use std::cell::RefCell;
use std::path::Path;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, IBindCtx,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, DefSubclassProc, GCS_VERBW, IContextMenu,
    IContextMenu2, IContextMenu3, IShellFolder, RemoveWindowSubclass, SHBindToParent,
    SHGetDesktopFolder, SHParseDisplayName, SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DeleteMenu, DestroyMenu, GetCursorPos, GetMenuItemCount,
    GetMenuItemID, GetMenuItemInfoW, MENUITEMINFOW, MF_BYPOSITION, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, MFT_SEPARATOR, MIIM_FTYPE, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
    WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
};
use windows::core::{Interface, PCSTR, PCWSTR, PSTR};

pub enum MenuOutcome {
    /// One of the caller's prepended items was picked (its id).
    Custom(u32),
    /// A shell verb ran — caller should refresh the listing.
    Invoked,
    Dismissed,
}

/// Caller-supplied menu entry: (id, label, enabled). Empty label = separator.
pub type CustomItem = (u32, &'static str, bool);

const ID_SHELL_FIRST: u32 = 0x1000;
const ID_SHELL_LAST: u32 = 0x7FFF;

// Verbs we already provide as custom entries (or that would break in-app UX,
// like "open" launching a real Explorer window for directories).
const ITEM_VERB_FILTER: &[&str] = &["open", "cut", "copy", "delete", "rename", "copyaspath"];
// Background: our 붙여넣기 works off the app clipboard, not OLE's.
const BG_VERB_FILTER: &[&str] = &["paste", "pastelink", "refresh"];

thread_local! {
    // Live only while TrackPopupMenuEx runs; the subclass proc forwards
    // menu messages here so shell submenus (새로 만들기 …) can populate.
    static MENU_FWD: RefCell<Option<(Option<IContextMenu2>, Option<IContextMenu3>)>> =
        const { RefCell::new(None) };
}

unsafe extern "system" fn menu_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        let handled = MENU_FWD.with(|f| {
            let f = f.borrow();
            let (cm2, cm3) = f.as_ref()?;
            if let Some(cm3) = cm3 {
                let mut lres = LRESULT(0);
                if unsafe { cm3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut lres)) }.is_ok() {
                    return Some(lres);
                }
            }
            if let Some(cm2) = cm2 {
                if unsafe { cm2.HandleMenuMsg(msg, wparam, lparam) }.is_ok() {
                    return Some(LRESULT(0));
                }
            }
            None
        });
        if let Some(lres) = handled {
            return lres;
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// IContextMenu of one file/folder item.
unsafe fn item_menu(path: &Path) -> windows::core::Result<IContextMenu> {
    let w = wide(&path.display().to_string());
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe {
        SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None)?;
        let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
        let out = SHBindToParent::<IShellFolder>(pidl, Some(&mut child)).and_then(|folder| {
            folder.GetUIObjectOf::<IContextMenu>(
                HWND::default(),
                &[child as *const ITEMIDLIST],
                None,
            )
        });
        CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
        out
    }
}

/// Background IContextMenu of a directory (붙여넣기/새로 만들기/extensions).
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

/// Delete shell entries whose canonical verb duplicates one of ours.
/// Submenus (id == u32::MAX) and extension items without verbs are kept.
unsafe fn filter_verbs(
    hmenu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    cm: &IContextMenu,
    kill: &[&str],
) {
    unsafe {
        let count = GetMenuItemCount(Some(hmenu));
        for i in (0..count).rev() {
            let id = GetMenuItemID(hmenu, i);
            if id == u32::MAX || id < ID_SHELL_FIRST {
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
        // Collapse separator runs left behind by the deletions.
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
        // Trailing separator.
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

unsafe fn run(
    hwnd_isize: isize,
    cm: IContextMenu,
    custom: &[CustomItem],
    verb_filter: &[&str],
) -> MenuOutcome {
    unsafe {
        let hwnd = HWND(hwnd_isize as *mut core::ffi::c_void);
        let Ok(hmenu) = CreatePopupMenu() else {
            return MenuOutcome::Dismissed;
        };
        for (id, label, enabled) in custom {
            if label.is_empty() {
                let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, None);
            } else {
                let flags = if *enabled {
                    MF_STRING
                } else {
                    MF_STRING | MF_GRAYED
                };
                let w = wide(label);
                let _ = AppendMenuW(hmenu, flags, *id as usize, PCWSTR(w.as_ptr()));
            }
        }
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
        let _ = SetWindowSubclass(hwnd, Some(menu_subclass_proc), 0x51DE, 0);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let picked = TrackPopupMenuEx(
            hmenu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        );
        let _ = RemoveWindowSubclass(hwnd, Some(menu_subclass_proc), 0x51DE);
        MENU_FWD.with(|f| *f.borrow_mut() = None);
        let _ = DestroyMenu(hmenu);

        let cmd = picked.0 as u32;
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

pub fn show_item_menu(hwnd: isize, path: &Path, custom: &[CustomItem]) -> MenuOutcome {
    unsafe {
        // winit already OleInitialize'd this thread; S_FALSE here is fine.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        match item_menu(path) {
            Ok(cm) => run(hwnd, cm, custom, ITEM_VERB_FILTER),
            Err(_) => MenuOutcome::Dismissed,
        }
    }
}

pub fn show_background_menu(hwnd: isize, dir: &Path, custom: &[CustomItem]) -> MenuOutcome {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        match background_menu(dir) {
            Ok(cm) => run(hwnd, cm, custom, BG_VERB_FILTER),
            Err(_) => MenuOutcome::Dismissed,
        }
    }
}
