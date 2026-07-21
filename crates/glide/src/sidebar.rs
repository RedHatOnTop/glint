// Left sidebar data: known folders (OneDrive-redirect-aware) + drives with capacity.
use std::path::PathBuf;

use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetLogicalDrives};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures,
    FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows::core::GUID;

pub struct QuickItem {
    pub label: &'static str,
    pub glyph: &'static str,
    pub path: PathBuf,
}

pub struct Drive {
    pub root: PathBuf,
    pub label: String,
    pub total: u64,
    pub free: u64,
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let pw = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = pw.to_string().ok()?;
        windows::Win32::System::Com::CoTaskMemFree(Some(pw.0 as *const _));
        Some(PathBuf::from(s))
    }
}

/// Quick-access entries, Fluent glyph per folder. Missing folders are skipped.
pub fn quick_items() -> Vec<QuickItem> {
    let defs: [(&str, &str, &GUID); 6] = [
        ("바탕 화면", "\u{E7F4}", &FOLDERID_Desktop),
        ("다운로드", "\u{E896}", &FOLDERID_Downloads),
        ("문서", "\u{E8A5}", &FOLDERID_Documents),
        ("사진", "\u{E91B}", &FOLDERID_Pictures),
        ("음악", "\u{E8D6}", &FOLDERID_Music),
        ("동영상", "\u{E714}", &FOLDERID_Videos),
    ];
    defs.into_iter()
        .filter_map(|(label, glyph, id)| {
            known_folder(id)
                .filter(|p| p.exists())
                .map(|path| QuickItem { label, glyph, path })
        })
        .collect()
}

fn favorites_file() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("glide").join("favorites.txt"))
}

/// Pinned folders, one absolute path per line. Missing file = empty list.
pub fn load_favorites() -> Vec<PathBuf> {
    let Some(file) = favorites_file() else {
        return Vec::new();
    };
    std::fs::read_to_string(file)
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

pub fn save_favorites(favs: &[PathBuf]) {
    let Some(file) = favorites_file() else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body: String = favs.iter().map(|p| format!("{}\n", p.display())).collect();
    let _ = std::fs::write(file, body);
}

/// Present drives with capacity info.
pub fn drives() -> Vec<Drive> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .filter_map(|i| {
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
            let (mut free, mut total) = (0u64, 0u64);
            let ok = unsafe {
                GetDiskFreeSpaceExW(
                    windows::core::PCWSTR(wide.as_ptr()),
                    None,
                    Some(&mut total),
                    Some(&mut free),
                )
            };
            if ok.is_err() || total == 0 {
                return None; // empty card readers etc.
            }
            Some(Drive {
                root: PathBuf::from(&root),
                label: format!("{letter}:"),
                total,
                free,
            })
        })
        .collect()
}
