//! Win+R. The stock box is shell32's `RunFileDlg`, which is modal — hosting it
//! on the bar thread would freeze the bar for as long as it is up, and reaching
//! it through `rundll32` (the way this shell used to) puts "RunDLL" in the
//! window's own description. This is ours: the panel language the rest of the
//! shell uses, on the thread that opened it, blocking nothing.
//!
//! History is explorer's `RunMRU`, read and written in explorer's own format,
//! so a machine that switches between shells keeps one list rather than two.

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ROUNDED_RECT};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_WORD_WRAPPING_NO_WRAP,
    IDWriteTextFormat,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, HBRUSH, HDC, SetBkColor, SetTextColor, ValidateRect,
};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetFocus, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use crate::render::{Renderer, fill_round, rect};
use crate::theme;

const W: f32 = 460.0;
const H: f32 = 190.0;
const PAD: f32 = 18.0;
/// The field is drawn here; the EDIT popup sits inside it, inset for padding
/// an EDIT has no way to draw for itself.
const FIELD_X: f32 = 58.0;
const FIELD_Y: f32 = 92.0;
const FIELD_H: f32 = 34.0;
const EDIT_X: f32 = FIELD_X + 8.0;
const EDIT_Y: f32 = FIELD_Y + 7.0;
const EDIT_W: f32 = W - PAD - 8.0 - EDIT_X;
const EDIT_H: f32 = 20.0;
const BTN_W: f32 = 96.0;
const BTN_H: f32 = 32.0;

/// The field's fill, as GDI wants it (BGR) and as D2D wants it, kept in step so
/// no seam shows where the EDIT's own background meets what we drew.
const FIELD_BGR: u32 = 0x0038_3532;
const FIELD_RGB: (u8, u8, u8) = (50, 53, 56);

/// The one box that may be up, so a second Win+R raises it instead of stacking.
static OPEN: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// The edit popup answers these to its owner; an EDIT keeps them to itself.
const WM_RUN_KEY: u32 = WM_APP + 21;
/// The edit lost the focus, which is this box's whole click-away signal — the
/// panel never takes activation, so it never hears the user leave.
const WM_RUN_CLOSE: u32 = WM_APP + 22;

/// Not in the crate's WindowsAndMessaging surface, same as in `desktop`.
const EM_SETSEL: u32 = 0x00B1;

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Ok,
    Cancel,
}

pub struct RunDialog {
    hwnd: HWND,
    edit: HWND,
    renderer: Renderer,
    fmt_head: IDWriteTextFormat,
    fmt_body: IDWriteTextFormat,
    fmt_btn: IDWriteTextFormat,
    fmt_glyph: IDWriteTextFormat,
    edit_brush: HBRUSH,
    scale: f32,
    /// explorer's list, newest first.
    history: Vec<String>,
    /// Where up/down currently sit in `history`; None is the typed line.
    hist_at: Option<usize>,
    error: Option<String>,
    hover: Option<Act>,
    /// Set by `close`, because launching something takes the foreground and
    /// the deactivation that follows would ask for a second close.
    closing: bool,
}

/// Open the run box, or raise the one already open. Returns at once — the
/// window lives on the calling thread's message loop and is modal to nothing.
pub fn open() {
    let existing = HWND(OPEN.load(std::sync::atomic::Ordering::Relaxed) as *mut _);
    if !existing.0.is_null() && unsafe { IsWindow(Some(existing)).as_bool() } {
        unsafe {
            let _ = SetForegroundWindow(existing);
        }
        return;
    }
    match RunDialog::new() {
        Ok(dlg) => unsafe {
            let hwnd = dlg.hwnd;
            let boxed = Box::into_raw(Box::new(dlg));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, boxed as isize);
            OPEN.store(hwnd.0 as isize, std::sync::atomic::Ordering::Relaxed);
            (*boxed).show();
        },
        Err(e) => crate::safety::note(&format!("run dialog: {e}")),
    }
}

impl RunDialog {
    fn new() -> anyhow::Result<Self> {
        unsafe {
            let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let class = w!("glide_shell_run");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            RegisterClassW(&wc);

            // WS_EX_NOACTIVATE: the edit popup holds the focus for the whole
            // box, and a click on 확인 must not take it away before the click
            // is handled. The panel is canvas, never the active window.
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP | WS_EX_NOACTIVATE,
                class,
                w!("실행"),
                WS_POPUP,
                0,
                0,
                64,
                64,
                None,
                None,
                Some(hinstance.into()),
                None,
            )?;
            let on: i32 = 1;
            let _ =
                DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as _, 4);
            let backdrop: i32 = 3; // DWMSBT_TRANSIENTWINDOW
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &backdrop as *const _ as _,
                4,
            );
            let corner = DWMWCP_ROUND.0;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as _,
                4,
            );

            let dpi = GetDpiForWindow(hwnd).max(96) as f32;
            let renderer = Renderer::new(hwnd, 64, 64, dpi)?;
            let mk = |family: PCWSTR, size: f32, weight: DWRITE_FONT_WEIGHT| {
                renderer.dwrite.CreateTextFormat(
                    family,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("ko-kr"),
                )
            };
            let ui = |size: f32, weight: DWRITE_FONT_WEIGHT| {
                mk(w!("Segoe UI Variable"), size, weight)
                    .or_else(|_| mk(w!("Segoe UI"), size, weight))
            };
            let fmt_head = ui(17.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
            fmt_head.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let fmt_body = ui(12.5, DWRITE_FONT_WEIGHT_NORMAL)?;
            let fmt_btn = ui(13.0, DWRITE_FONT_WEIGHT_NORMAL)?;
            fmt_btn.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            fmt_btn.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_btn.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            let fmt_glyph = mk(w!("Segoe Fluent Icons"), 18.0, DWRITE_FONT_WEIGHT_NORMAL)
                .or_else(|_| mk(w!("Segoe MDL2 Assets"), 18.0, DWRITE_FONT_WEIGHT_NORMAL))?;
            fmt_glyph.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt_glyph.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            let scale = dpi / 96.0;
            // A real EDIT rather than a text field of our own: it brings the
            // IME and the whole editing vocabulary — selection, clipboard,
            // undo — with it, the same trade the desktop's rename box made.
            //
            // Owned popup, not a child. WS_EX_NOREDIRECTIONBITMAP leaves the
            // panel with no redirection surface for a child window to paint
            // into, and a child EDIT on it is simply never drawn.
            let edit = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                w!("EDIT"),
                w!(""),
                WS_POPUP | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                0,
                0,
                10,
                10,
                Some(hwnd),
                None,
                Some(hinstance.into()),
                None,
            )?;
            SendMessageW(
                edit,
                WM_SETFONT,
                Some(WPARAM(crate::desktop::ui_font(scale).0 as usize)),
                Some(LPARAM(1)),
            );
            let _ = SetWindowSubclass(edit, Some(edit_proc), 0x0B0C, hwnd.0 as usize);

            Ok(Self {
                hwnd,
                edit,
                renderer,
                fmt_head,
                fmt_body,
                fmt_btn,
                fmt_glyph,
                edit_brush: CreateSolidBrush(COLORREF(FIELD_BGR)),
                scale,
                history: read_mru(),
                hist_at: None,
                error: None,
                hover: None,
                closing: false,
            })
        }
    }

    fn show(&mut self) {
        unsafe {
            let (dw, dh) = ((W * self.scale) as i32, (H * self.scale) as i32);
            let mut work = windows::Win32::Foundation::RECT::default();
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut work as *mut _ as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            // The corner the start button is in, which is where the stock box
            // has always opened — not the middle of the screen.
            let margin = (16.0 * self.scale) as i32;
            let (x, y) = (work.left + margin, work.bottom - dh - margin);
            let _ = SetWindowPos(
                self.hwnd,
                None,
                x,
                y,
                dw,
                dh,
                SWP_NOZORDER | SWP_SHOWWINDOW | SWP_NOACTIVATE,
            );
            let _ = self
                .renderer
                .resize(dw as u32, dh as u32, self.scale * 96.0);
            // Screen coordinates: the edit is a popup of its own, above the
            // panel, not inside it.
            let _ = SetWindowPos(
                self.edit,
                Some(HWND_TOP),
                x + (EDIT_X * self.scale) as i32,
                y + (EDIT_Y * self.scale) as i32,
                (EDIT_W * self.scale) as i32,
                (EDIT_H * self.scale) as i32,
                SWP_SHOWWINDOW,
            );
            let _ = SetForegroundWindow(self.edit);
            let _ = SetFocus(Some(self.edit));
            self.paint();
        }
    }

    fn ok_rect(&self) -> D2D_RECT_F {
        let y = H - PAD - BTN_H;
        rect(
            W - PAD - BTN_W * 2.0 - 10.0,
            y,
            W - PAD - BTN_W - 10.0,
            y + BTN_H,
        )
    }

    fn cancel_rect(&self) -> D2D_RECT_F {
        let y = H - PAD - BTN_H;
        rect(W - PAD - BTN_W, y, W - PAD, y + BTN_H)
    }

    fn hit(&self, x: f32, y: f32) -> Option<Act> {
        let inside = |r: D2D_RECT_F| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        if inside(self.ok_rect()) {
            Some(Act::Ok)
        } else if inside(self.cancel_rect()) {
            Some(Act::Cancel)
        } else {
            None
        }
    }

    fn paint(&mut self) {
        unsafe {
            let r = &self.renderer;
            r.dc.BeginDraw();
            r.dc.Clear(Some(&theme::rgba(0, 0, 0, 0.0)));

            let panel = rect(0.0, 0.0, W, H);
            fill_round(r, panel, 12.0, theme::rgba(30, 31, 37, 0.97));
            if let Ok(b) = r.brush(theme::rgba(255, 255, 255, 0.09)) {
                r.dc.DrawRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: panel,
                        radiusX: 12.0,
                        radiusY: 12.0,
                    },
                    &b,
                    1.0,
                    None,
                );
            }

            let badge = rect(PAD, 18.0, PAD + 40.0, 58.0);
            fill_round(r, badge, 10.0, theme::with_alpha(theme::accent(), 0.22));
            self.text("\u{E756}", &self.fmt_glyph, badge, theme::TEXT);

            self.text("실행", &self.fmt_head, rect(70.0, 19.0, W - PAD, 43.0), theme::TEXT);
            let (desc, color) = match &self.error {
                Some(e) => (e.clone(), theme::rgba(232, 118, 108, 1.0)),
                None => (
                    "프로그램, 폴더, 문서, 인터넷 주소를 입력하세요.".to_string(),
                    theme::TEXT_DIM,
                ),
            };
            self.text(&desc, &self.fmt_body, rect(70.0, 43.0, W - PAD, 66.0), color);

            let field = rect(FIELD_X, FIELD_Y, W - PAD, FIELD_Y + FIELD_H);
            let (fr, fg, fb) = FIELD_RGB;
            fill_round(r, field, 6.0, theme::rgba(fr, fg, fb, 1.0));
            if let Ok(b) = r.brush(theme::rgba(255, 255, 255, 0.10)) {
                r.dc.DrawRoundedRectangle(
                    &D2D1_ROUNDED_RECT { rect: field, radiusX: 6.0, radiusY: 6.0 },
                    &b,
                    1.0,
                    None,
                );
            }
            self.text(
                "열기",
                &self.fmt_body,
                rect(PAD, FIELD_Y + 8.0, FIELD_X - 6.0, FIELD_Y + FIELD_H),
                theme::TEXT_DIM,
            );
            self.text(
                "Ctrl+Shift+Enter 관리자 권한",
                &self.fmt_body,
                rect(PAD, H - PAD - BTN_H + 8.0, W - PAD - BTN_W * 2.0 - 20.0, H - PAD),
                theme::rgba(148, 152, 162, 0.75),
            );

            for (act, rc, label) in [
                (Act::Ok, self.ok_rect(), "확인"),
                (Act::Cancel, self.cancel_rect(), "취소"),
            ] {
                let base = if act == Act::Ok {
                    theme::accent()
                } else {
                    theme::rgba(255, 255, 255, 0.08)
                };
                let fill = if self.hover == Some(act) {
                    theme::with_alpha(base, (base.a + 0.14).min(1.0))
                } else {
                    base
                };
                fill_round(r, rc, 6.0, fill);
                self.text(label, &self.fmt_btn, rc, theme::TEXT);
            }

            let _ = r.dc.EndDraw(None, None);
            let _ = r.present();
            let _ = ValidateRect(Some(self.hwnd), None);
        }
    }

    fn text(&self, s: &str, fmt: &IDWriteTextFormat, rc: D2D_RECT_F, color: D2D1_COLOR_F) {
        let t: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            if let Ok(b) = self.renderer.brush(color) {
                self.renderer.dc.DrawText(
                    &t,
                    fmt,
                    &rc,
                    &b,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
    }

    fn line(&self) -> String {
        unsafe {
            let n = GetWindowTextLengthW(self.edit);
            if n <= 0 {
                return String::new();
            }
            let mut buf = vec![0u16; n as usize + 1];
            let got = GetWindowTextW(self.edit, &mut buf);
            String::from_utf16_lossy(&buf[..got as usize])
        }
    }

    fn set_line(&self, s: &str) {
        let t = wide(s);
        let end = s.encode_utf16().count();
        unsafe {
            let _ = SetWindowTextW(self.edit, PCWSTR(t.as_ptr()));
            // Caret at the end with nothing selected: a history entry is a
            // starting point to edit, not a value the next keystroke wipes.
            SendMessageW(
                self.edit,
                EM_SETSEL,
                Some(WPARAM(end)),
                Some(LPARAM(end as isize)),
            );
        }
    }

    /// Up walks back through the history, down forward; past the newest is the
    /// blank line the box opened with.
    fn step_history(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        self.hist_at = match (self.hist_at, back) {
            (None, true) => Some(0),
            (None, false) => None,
            (Some(i), true) => Some((i + 1).min(self.history.len() - 1)),
            (Some(0), false) => None,
            (Some(i), false) => Some(i - 1),
        };
        match self.hist_at {
            Some(i) => self.set_line(&self.history[i].clone()),
            None => self.set_line(""),
        }
    }

    /// True when the box has done its job and should go away.
    fn accept(&mut self, admin: bool) -> bool {
        let line = self.line().trim().to_string();
        if line.is_empty() {
            return false;
        }
        if launch(&line, admin) {
            write_mru(&line);
            return true;
        }
        self.error = Some(format!("'{line}'을(를) 찾을 수 없습니다."));
        self.paint();
        false
    }

    fn close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        unsafe {
            OPEN.store(0, std::sync::atomic::Ordering::Relaxed);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

impl Drop for RunDialog {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.edit_brush.into());
        }
    }
}

/// ShellExecute, with the line split the way the stock box splits it: the first
/// token names the thing to run and the rest are its arguments, unless the
/// whole line is a path that exists.
fn launch(line: &str, admin: bool) -> bool {
    let expanded = expand(line);
    let (file, args) = split_args(&expanded);
    let fw = wide(&file);
    let aw = wide(&args);
    unsafe {
        let h = ShellExecuteW(
            None,
            if admin { w!("runas") } else { w!("open") },
            PCWSTR(fw.as_ptr()),
            if args.is_empty() {
                PCWSTR::null()
            } else {
                PCWSTR(aw.as_ptr())
            },
            None,
            SW_SHOWNORMAL,
        );
        // Anything over 32 succeeded; every failure code is small, and the
        // return value is the only signal ShellExecute gives.
        h.0 as isize > 32
    }
}

fn split_args(line: &str) -> (String, String) {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix('"') {
        return match rest.split_once('"') {
            Some((exe, args)) => (exe.to_string(), args.trim().to_string()),
            None => (rest.to_string(), String::new()),
        };
    }
    // An unquoted path with spaces in it is one whole thing if it exists.
    if std::path::Path::new(line).exists() {
        return (line.to_string(), String::new());
    }
    match line.split_once(' ') {
        Some((exe, args)) => (exe.to_string(), args.trim().to_string()),
        None => (line.to_string(), String::new()),
    }
}

fn expand(s: &str) -> String {
    let src = wide(s);
    unsafe {
        let n = ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), None);
        if n == 0 {
            return s.to_string();
        }
        let mut buf = vec![0u16; n as usize];
        let got = ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf));
        if got == 0 {
            return s.to_string();
        }
        String::from_utf16_lossy(&buf[..(got as usize).saturating_sub(1)])
    }
}

const MRU_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\RunMRU";

/// explorer's list: values "a".."z", each ending in \1, ordered by MRUList.
fn read_mru() -> Vec<String> {
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(MRU_KEY) else {
        return Vec::new();
    };
    let order: String = key.get_value("MRUList").unwrap_or_default();
    order
        .chars()
        .filter_map(|c| key.get_value::<String, _>(c.to_string()).ok())
        .map(|v| v.trim_end_matches('\u{1}').to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

fn write_mru(line: &str) {
    let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(MRU_KEY) else {
        return;
    };
    let order: String = key.get_value("MRUList").unwrap_or_default();
    // The slot this command already holds, else the first letter not in use,
    // else the one that has fallen to the end of the list.
    let slot = order
        .chars()
        .find(|c| {
            key.get_value::<String, _>(c.to_string())
                .is_ok_and(|v| v.trim_end_matches('\u{1}') == line)
        })
        .or_else(|| ('a'..='z').find(|c| !order.contains(*c)))
        .or_else(|| order.chars().last())
        .unwrap_or('a');
    let _ = key.set_value(slot.to_string(), &format!("{line}\u{1}"));
    let rest: String = order.chars().filter(|c| *c != slot).collect();
    let _ = key.set_value("MRUList", &format!("{slot}{rest}"));
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The four keys a single-line EDIT keeps to itself. Posted rather than
/// answered here, because two of the answers destroy this window's parent.
unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    owner: usize,
) -> LRESULT {
    unsafe {
        let parent = HWND(owner as *mut core::ffi::c_void);
        match msg {
            WM_KEYDOWN
                if [VK_RETURN.0, VK_ESCAPE.0, VK_UP.0, VK_DOWN.0].contains(&(wparam.0 as u16)) =>
            {
                let _ = PostMessageW(Some(parent), WM_RUN_KEY, wparam, LPARAM(0));
                LRESULT(0)
            }
            // The bell an EDIT rings for the two of those that reach WM_CHAR.
            WM_CHAR if wparam.0 as u16 == 0x0D || wparam.0 as u16 == 0x1B => LRESULT(0),
            WM_KILLFOCUS => {
                let _ = PostMessageW(Some(parent), WM_RUN_CLOSE, WPARAM(0), LPARAM(0));
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut RunDialog;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let dlg = &mut *ptr;
        let admin = || GetKeyState(VK_CONTROL.0 as i32) < 0 && GetKeyState(VK_SHIFT.0 as i32) < 0;
        let pt = |scale: f32| {
            (
                (lparam.0 & 0xFFFF) as i16 as f32 / scale,
                ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / scale,
            )
        };
        match msg {
            WM_PAINT => {
                dlg.paint();
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            // A stock control on a dark panel paints itself white without this,
            // and the box looks half finished.
            WM_CTLCOLOREDIT => {
                SetTextColor(HDC(wparam.0 as *mut _), COLORREF(0x00E8_E9E8));
                SetBkColor(HDC(wparam.0 as *mut _), COLORREF(FIELD_BGR));
                LRESULT(dlg.edit_brush.0 as isize)
            }
            WM_RUN_KEY => {
                match wparam.0 as u16 {
                    k if k == VK_RETURN.0 => {
                        if dlg.accept(admin()) {
                            dlg.close();
                        }
                    }
                    k if k == VK_ESCAPE.0 => dlg.close(),
                    k if k == VK_UP.0 => dlg.step_history(true),
                    _ => dlg.step_history(false),
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = pt(dlg.scale);
                let h = dlg.hit(x, y);
                if h != dlg.hover {
                    dlg.hover = h;
                    dlg.paint();
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = pt(dlg.scale);
                match dlg.hit(x, y) {
                    Some(Act::Ok) => {
                        if dlg.accept(admin()) {
                            dlg.close();
                        }
                    }
                    Some(Act::Cancel) => dlg.close(),
                    None => {
                        let _ = SetFocus(Some(dlg.edit));
                    }
                }
                LRESULT(0)
            }
            WM_RUN_CLOSE => {
                dlg.close();
                LRESULT(0)
            }
            WM_DESTROY => {
                OPEN.store(0, std::sync::atomic::Ordering::Relaxed);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
