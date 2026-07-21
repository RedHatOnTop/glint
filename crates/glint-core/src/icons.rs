// Shell icon extraction (SHGetFileInfoW → HICON → RGBA → egui texture), async + cached.
//
// Two key modes:
//  - Path: real per-file icon (exe/lnk — used by the launcher, and special files)
//  - Ext/Dir: extension-keyed via SHGFI_USEFILEATTRIBUTES — no disk touch, so a
//    100k-file directory costs one shell call per distinct extension, not per file.
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

#[derive(Clone, Debug)]
pub enum IconKey {
    /// Real icon of this concrete file (touches the shell/file).
    Path(String),
    /// Generic icon for an extension, e.g. "pdf" (no disk access).
    Ext(String),
    /// Generic folder icon.
    Dir,
}

impl IconKey {
    fn cache_key(&self) -> String {
        match self {
            IconKey::Path(p) => format!("P|{p}"),
            IconKey::Ext(e) => format!("E|{}", e.to_lowercase()),
            IconKey::Dir => "D|".into(),
        }
    }
}

pub struct IconCache {
    map: Arc<Mutex<HashMap<String, Option<egui::TextureHandle>>>>,
    tx: mpsc::Sender<IconKey>,
}

impl IconCache {
    pub fn spawn(ctx: egui::Context) -> Self {
        let map: Arc<Mutex<HashMap<String, Option<egui::TextureHandle>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel::<IconKey>();
        let m = map.clone();
        std::thread::spawn(move || {
            while let Ok(key) = rx.recv() {
                let ck = key.cache_key();
                if m.lock().unwrap().get(&ck).is_some_and(|v| v.is_some()) {
                    continue;
                }
                let tex = extract(&key).map(|img| {
                    ctx.load_texture(format!("icon:{ck}"), img, egui::TextureOptions::LINEAR)
                });
                m.lock().unwrap().insert(ck, tex);
                ctx.request_repaint();
            }
        });
        Self { map, tx }
    }

    /// Returns the texture if ready; otherwise queues extraction.
    pub fn get(&self, key: IconKey) -> Option<egui::TextureHandle> {
        let ck = key.cache_key();
        let mut map = self.map.lock().unwrap();
        match map.get(&ck) {
            Some(t) => t.clone(),
            None => {
                map.insert(ck, None);
                let _ = self.tx.send(key);
                None
            }
        }
    }

    /// Ext-keyed icon for a directory listing entry (fast path), except real
    /// per-file icons where they actually differ (exe/lnk/ico).
    pub fn get_for_entry(&self, path: &str, is_dir: bool) -> Option<egui::TextureHandle> {
        // A drive root ("C:\") is is_dir=true but must NOT read as a folder — pull
        // its real shell icon (the drive glyph, with type overlay).
        if is_drive_root(path) {
            return self.get(IconKey::Path(path.to_string()));
        }
        if is_dir {
            return self.get(IconKey::Dir);
        }
        let ext = std::path::Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "exe" | "lnk" | "ico" => self.get(IconKey::Path(path.to_string())),
            "" => self.get(IconKey::Ext("txt".into())),
            _ => self.get(IconKey::Ext(ext)),
        }
    }
}

/// True for a bare drive root like `C:\` (or `C:`). These are directories to the
/// filesystem but Explorer draws them as drives, not folders.
pub fn is_drive_root(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() == 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || (b.len() == 2 && b[1] == b':')
}

fn extract(key: &IconKey) -> Option<egui::ColorImage> {
    let (name, attrs, use_attrs) = match key {
        IconKey::Path(p) => (p.clone(), FILE_ATTRIBUTE_NORMAL, false),
        IconKey::Ext(e) => (format!("x.{e}"), FILE_ATTRIBUTE_NORMAL, true),
        IconKey::Dir => ("x".to_string(), FILE_ATTRIBUTE_DIRECTORY, true),
    };
    shell_icon_rgba(&name, attrs, use_attrs)
}

fn shell_icon_rgba(
    name: &str,
    attrs: FILE_FLAGS_AND_ATTRIBUTES,
    use_attrs: bool,
) -> Option<egui::ColorImage> {
    unsafe {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sfi = SHFILEINFOW::default();
        let mut flags = SHGFI_ICON | SHGFI_LARGEICON;
        if use_attrs {
            flags |= SHGFI_USEFILEATTRIBUTES;
        }
        let ok = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            attrs,
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        if ok == 0 || sfi.hIcon.is_invalid() {
            return None;
        }
        let hicon = sfi.hIcon;

        let mut info = ICONINFO::default();
        if GetIconInfo(hicon, &mut info).is_err() {
            let _ = DestroyIcon(hicon);
            return None;
        }

        let hdc = GetDC(None);
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        // First call: fetch dimensions
        GetDIBits(hdc, info.hbmColor, 0, 0, None, &mut bi, DIB_RGB_COLORS);
        let w = bi.bmiHeader.biWidth;
        let h = bi.bmiHeader.biHeight.abs();
        let mut out: Option<egui::ColorImage> = None;
        if w > 0 && h > 0 && w <= 512 && h <= 512 {
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;
            bi.bmiHeader.biHeight = -h; // top-down
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let got = GetDIBits(
                hdc,
                info.hbmColor,
                0,
                h as u32,
                Some(buf.as_mut_ptr() as *mut _),
                &mut bi,
                DIB_RGB_COLORS,
            );
            if got != 0 {
                // BGRA → RGBA
                for px in buf.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                // Fully-transparent alpha channel means an old-style mask icon; treat as opaque.
                if buf.chunks_exact(4).all(|p| p[3] == 0) {
                    for px in buf.chunks_exact_mut(4) {
                        px[3] = 255;
                    }
                }
                out = Some(egui::ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    &buf,
                ));
            }
        }
        ReleaseDC(None, hdc);
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        let _ = DestroyIcon(hicon);
        out
    }
}
