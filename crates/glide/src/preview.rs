// Shell thumbnails (IShellItemImageFactory) — the same previews Explorer shows,
// so images, video frames, PDFs etc. all work via installed thumbnail providers.
// Extraction runs on a dedicated STA worker; results land in a texture cache
// keyed by path+mtime so edits invalidate.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};
use std::time::SystemTime;

use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, HBITMAP,
    HGDIOBJ, ReleaseDC,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY,
};
use windows::core::PCWSTR;

fn cache_key(path: &str, mtime: Option<SystemTime>) -> String {
    let t = mtime
        .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{path}|{t}")
}

pub struct ThumbCache {
    map: Arc<Mutex<HashMap<String, Option<egui::TextureHandle>>>>,
    tx: mpsc::Sender<(String, String)>, // (path, cache key)
}

impl ThumbCache {
    pub fn spawn(ctx: egui::Context) -> Self {
        let map: Arc<Mutex<HashMap<String, Option<egui::TextureHandle>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel::<(String, String)>();
        let m = map.clone();
        std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            while let Ok((path, key)) = rx.recv() {
                if m.lock().unwrap().get(&key).is_some_and(|v| v.is_some()) {
                    continue;
                }
                let tex = thumbnail_rgba(&path).map(|img| {
                    ctx.load_texture(format!("thumb:{key}"), img, egui::TextureOptions::LINEAR)
                });
                m.lock().unwrap().insert(key, tex);
                ctx.request_repaint();
            }
        });
        Self { map, tx }
    }

    /// None = queued/pending, Some(None) = no thumbnail for this file type,
    /// Some(Some(tex)) = ready.
    pub fn get(
        &self,
        path: &str,
        mtime: Option<SystemTime>,
    ) -> Option<Option<egui::TextureHandle>> {
        let key = cache_key(path, mtime);
        let mut map = self.map.lock().unwrap();
        match map.get(&key) {
            Some(t) => Some(t.clone()),
            None => {
                map.insert(key.clone(), None);
                let _ = self.tx.send((path.to_string(), key));
                None
            }
        }
    }
}

fn thumbnail_rgba(path: &str) -> Option<egui::ColorImage> {
    unsafe {
        let w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let item: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        let hbm = item
            .GetImage(
                SIZE { cx: 256, cy: 256 },
                SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK,
            )
            .ok()?;
        let out = hbitmap_rgba(hbm);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        out
    }
}

unsafe fn hbitmap_rgba(hbm: HBITMAP) -> Option<egui::ColorImage> {
    unsafe {
        let hdc = GetDC(None);
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        GetDIBits(hdc, hbm, 0, 0, None, &mut bi, DIB_RGB_COLORS);
        let w = bi.bmiHeader.biWidth;
        let h = bi.bmiHeader.biHeight.abs();
        let mut out: Option<egui::ColorImage> = None;
        if w > 0 && h > 0 && w <= 1024 && h <= 1024 {
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;
            bi.bmiHeader.biHeight = -h; // top-down
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let got = GetDIBits(
                hdc,
                hbm,
                0,
                h as u32,
                Some(buf.as_mut_ptr() as *mut _),
                &mut bi,
                DIB_RGB_COLORS,
            );
            if got != 0 {
                // BGRA → RGBA; thumbnails are usually opaque with alpha 0 —
                // treat an all-zero alpha channel as opaque.
                for px in buf.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
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
        out
    }
}

/// Text-ish file extensions the details panel previews as a content snippet
/// (shell thumbnails only cover images/video/PDF/office — text has none).
pub fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "md"
            | "markdown"
            | "rst"
            | "log"
            | "csv"
            | "tsv"
            | "json"
            | "jsonc"
            | "toml"
            | "yaml"
            | "yml"
            | "ini"
            | "cfg"
            | "conf"
            | "properties"
            | "env"
            | "xml"
            | "html"
            | "htm"
            | "svg"
            | "css"
            | "scss"
            | "less"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "jsx"
            | "tsx"
            | "rs"
            | "py"
            | "go"
            | "rb"
            | "php"
            | "lua"
            | "pl"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "cc"
            | "cxx"
            | "cs"
            | "java"
            | "kt"
            | "kts"
            | "swift"
            | "sh"
            | "bash"
            | "zsh"
            | "bat"
            | "cmd"
            | "ps1"
            | "psm1"
            | "sql"
            | "gradle"
            | "make"
            | "cmake"
            | "dockerfile"
            | "gitignore"
            | "gitattributes"
            | "editorconfig"
            | "lock"
            | "tex"
            | "bib"
            | "srt"
            | "vtt"
            | "diff"
            | "patch"
    )
}

/// Read the first few KB of a text file as a preview snippet. Returns `None`
/// for unreadable or binary content (a NUL byte means the extension lied).
pub fn read_snippet(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 8192];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    if buf.contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&buf);
    let mut out = text.lines().take(80).collect::<Vec<_>>().join("\n");
    if out.len() > 4000 {
        out.truncate(4000);
    }
    Some(out)
}
