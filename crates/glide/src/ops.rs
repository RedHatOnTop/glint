// Shell file operations — native progress/conflict/undo UI via SHFileOperationW.
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, IDataObject,
};
use windows::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    FO_COPY, FO_DELETE, FO_MOVE, FOF_ALLOWUNDO, FOF_RENAMEONCOLLISION, IShellFolder,
    SHBindToParent, SHDoDragDrop, SHFILEOPSTRUCTW, SHFileOperationW, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowRect, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::PCWSTR;

// pFrom/pTo want double-null-terminated path lists.
fn double_null(path: &Path) -> Vec<u16> {
    let mut v: Vec<u16> = path.as_os_str().encode_wide().collect();
    v.push(0);
    v.push(0);
    v
}

// A double-null-terminated list of several source paths.
fn multi_null(paths: &[&Path]) -> Vec<u16> {
    let mut v: Vec<u16> = Vec::new();
    for p in paths {
        v.extend(p.as_os_str().encode_wide());
        v.push(0);
    }
    v.push(0); // extra terminator closing the list
    v
}

// Many sources → one dest dir in a single shell call (one native progress/undo).
fn run_many(func: u32, from: &[&Path], to: Option<&Path>, flags: u16) -> bool {
    if from.is_empty() {
        return true;
    }
    let from_buf = multi_null(from);
    let to_buf = to.map(double_null);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: func,
        pFrom: PCWSTR(from_buf.as_ptr()),
        pTo: to_buf
            .as_ref()
            .map(|b| PCWSTR(b.as_ptr()))
            .unwrap_or(PCWSTR::null()),
        fFlags: flags,
        ..Default::default()
    };
    let rc = unsafe { SHFileOperationW(&mut op) };
    rc == 0 && !op.fAnyOperationsAborted.as_bool()
}

/// Copy every `src` into directory `dest_dir` in one operation.
pub fn shell_copy_many(srcs: &[&Path], dest_dir: &Path, rename_on_collision: bool) -> bool {
    let flags = if rename_on_collision {
        FOF_RENAMEONCOLLISION.0 as u16
    } else {
        0
    };
    run_many(FO_COPY, srcs, Some(dest_dir), flags)
}

/// Move every `src` into directory `dest_dir` in one operation.
pub fn shell_move_many(srcs: &[&Path], dest_dir: &Path) -> bool {
    run_many(FO_MOVE, srcs, Some(dest_dir), 0)
}

/// Send every `path` to the recycle bin in one operation.
pub fn shell_recycle_many(paths: &[&Path]) -> bool {
    run_many(FO_DELETE, paths, None, FOF_ALLOWUNDO.0 as u16)
}

/// Create "새 폴더" (or "새 폴더 (2)"…) in `dir`; returns the created path.
pub fn create_new_folder(dir: &Path) -> std::io::Result<PathBuf> {
    let mut name = "새 폴더".to_string();
    let mut n = 2;
    while dir.join(&name).exists() {
        name = format!("새 폴더 ({n})");
        n += 1;
    }
    let path = dir.join(&name);
    std::fs::create_dir(&path)?;
    Ok(path)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// True when the mouse pointer is outside `hwnd`'s window rectangle.
/// Used to hand an in-progress drag off to the OLE drag source.
pub fn cursor_outside_window(hwnd: isize) -> bool {
    unsafe {
        let h = HWND(hwnd as *mut core::ffi::c_void);
        let mut pt = POINT::default();
        let mut rc = RECT::default();
        if GetCursorPos(&mut pt).is_err() || GetWindowRect(h, &mut rc).is_err() {
            return false;
        }
        pt.x < rc.left || pt.x > rc.right || pt.y < rc.top || pt.y > rc.bottom
    }
}

/// Restore (if minimized) and foreground the window — used when another glide
/// launch hands its folders to this instance.
pub fn bring_to_front(hwnd: isize) {
    unsafe {
        let h = HWND(hwnd as *mut core::ffi::c_void);
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        let _ = SetForegroundWindow(h);
    }
}

/// Start a native OLE drag of `paths` so they can be dropped into other apps
/// (Explorer, editors, chat…). Blocks — SHDoDragDrop runs its own modal loop —
/// and returns when the user drops or cancels. Must be called on the UI thread
/// (winit has already OleInitialize'd it) with the mouse button still down.
pub fn ole_drag_out(hwnd: isize, paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // Selected items all live in the current folder → one parent, N children.
        let mut abs_pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        let mut children: Vec<*const ITEMIDLIST> = Vec::new();
        let mut folder: Option<IShellFolder> = None;
        for p in paths {
            let w = wide(&p.display().to_string());
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None).is_err() {
                continue;
            }
            let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
            match SHBindToParent::<IShellFolder>(pidl, Some(&mut child)) {
                Ok(f) => {
                    if folder.is_none() {
                        folder = Some(f);
                    }
                    abs_pidls.push(pidl); // keep alive: `child` points into it
                    children.push(child as *const ITEMIDLIST);
                }
                Err(_) => CoTaskMemFree(Some(pidl as *const core::ffi::c_void)),
            }
        }
        let ok = (|| -> Option<()> {
            let folder = folder.as_ref()?;
            let data: IDataObject = folder
                .GetUIObjectOf(HWND(hwnd as *mut core::ffi::c_void), &children, None)
                .ok()?;
            let _ = SHDoDragDrop(
                Some(HWND(hwnd as *mut core::ffi::c_void)),
                &data,
                None,
                DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
            );
            Some(())
        })()
        .is_some();
        for pidl in abs_pidls {
            CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
        }
        ok
    }
}
