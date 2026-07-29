//! HICON → premultiplied-BGRA → ID2D1Bitmap1, for taskbar buttons.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_PROPERTIES1, ID2D1Bitmap1, ID2D1DeviceContext,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GCLP_HICON, GCLP_HICONSM, GetClassLongPtrW, GetIconInfo, HICON, ICON_BIG, ICON_SMALL,
    ICON_SMALL2, ICONINFO, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_GETICON,
};

/// Best-effort icon for a top-level window. None → caller draws a fallback.
pub fn window_icon(dc: &ID2D1DeviceContext, hwnd: HWND) -> Option<ID2D1Bitmap1> {
    let hicon = query_hicon(hwnd)?;
    hicon_to_bitmap(dc, hicon)
}

/// Convert a borrowed HICON (owned by another process — tray senders) without
/// destroying it. GetIconInfo copies the bitmaps, so no lifetime issues.
pub fn hicon_bitmap(dc: &ID2D1DeviceContext, hicon: HICON) -> Option<ID2D1Bitmap1> {
    if hicon.is_invalid() {
        return None;
    }
    hicon_to_bitmap(dc, hicon)
}

/// Icon extracted from an exe on disk, for pinned launchers. Unlike window
/// icons (owned by the target app), this HICON is ours and must be destroyed.
pub fn exe_icon(dc: &ID2D1DeviceContext, path: &str) -> Option<ID2D1Bitmap1> {
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW};
    use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;
    use windows::core::PCWSTR;
    unsafe {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sfi = SHFILEINFOW::default();
        let ok = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if ok == 0 || sfi.hIcon.is_invalid() {
            return None;
        }
        let bmp = hicon_to_bitmap(dc, sfi.hIcon);
        let _ = DestroyIcon(sfi.hIcon);
        bmp
    }
}

fn query_hicon(hwnd: HWND) -> Option<HICON> {
    unsafe {
        for kind in [ICON_SMALL2, ICON_SMALL, ICON_BIG] {
            let mut out: usize = 0;
            let _ = SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                WPARAM(kind as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                100,
                Some(&mut out),
            );
            if out != 0 {
                return Some(HICON(out as *mut _));
            }
        }
        for kind in [GCLP_HICONSM, GCLP_HICON] {
            let h = GetClassLongPtrW(hwnd, kind);
            if h != 0 {
                return Some(HICON(h as *mut _));
            }
        }
        None
    }
}

/// Menu item image (MIIM_BITMAP hbmpItem), for the own-drawn context menu.
/// The caller owns the bitmap — the shell keeps it for the menu's lifetime —
/// so nothing is freed here.
pub fn hbitmap_bitmap(dc: &ID2D1DeviceContext, hbm: HBITMAP) -> Option<ID2D1Bitmap1> {
    let (w, h, mut pixels) = dib_pixels(hbm)?;
    opaque_if_alphaless(&mut pixels);
    // Shell menu bitmaps arrive premultiplied, old extensions' do not.
    // Multiplying twice darkens every edge, so the pixels get asked.
    if pixels.chunks_exact(4).any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3]) {
        premultiply(&mut pixels);
    }
    make_bitmap(dc, w, h, &pixels)
}

fn hicon_to_bitmap(dc: &ID2D1DeviceContext, hicon: HICON) -> Option<ID2D1Bitmap1> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(hicon, &mut info).ok()?;
        // Both bitmaps must be freed regardless of which paths succeed.
        let color = info.hbmColor;
        let mask = info.hbmMask;
        let result = (|| {
            if color.is_invalid() {
                return None; // monochrome/mask-only icon — not worth rendering
            }
            let (w, h, mut pixels) = dib_pixels(color)?;
            opaque_if_alphaless(&mut pixels);
            // An icon's colour bitmap is straight alpha, always.
            premultiply(&mut pixels);
            make_bitmap(dc, w, h, &pixels)
        })();
        if !color.is_invalid() {
            let _ = DeleteObject(color.into());
        }
        if !mask.is_invalid() {
            let _ = DeleteObject(mask.into());
        }
        result
    }
}

/// A GDI bitmap's pixels as top-down BGRA.
fn dib_pixels(hbm: HBITMAP) -> Option<(i32, i32, Vec<u8>)> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(
            hbm.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        ) == 0
        {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        if w <= 0 || h <= 0 || w > 512 || h > 512 {
            return None;
        }
        let hdc = CreateCompatibleDC(None);
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let got = GetDIBits(
            hdc,
            hbm,
            0,
            h as u32,
            Some(pixels.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(hdc);
        if got == 0 { None } else { Some((w, h, pixels)) }
    }
}

/// Legacy 24-bit sources carry no alpha channel; treat them as opaque.
fn opaque_if_alphaless(pixels: &mut [u8]) {
    if pixels.chunks_exact(4).all(|p| p[3] == 0) {
        for p in pixels.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
}

fn premultiply(pixels: &mut [u8]) {
    for p in pixels.chunks_exact_mut(4) {
        let a = p[3] as u32;
        p[0] = ((p[0] as u32 * a) / 255) as u8;
        p[1] = ((p[1] as u32 * a) / 255) as u8;
        p[2] = ((p[2] as u32 * a) / 255) as u8;
    }
}

fn make_bitmap(dc: &ID2D1DeviceContext, w: i32, h: i32, pixels: &[u8]) -> Option<ID2D1Bitmap1> {
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        ..Default::default()
    };
    unsafe {
        dc.CreateBitmap(
            windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_U {
                width: w as u32,
                height: h as u32,
            },
            Some(pixels.as_ptr() as *const _),
            (w * 4) as u32,
            &props,
        )
        .ok()
    }
}
