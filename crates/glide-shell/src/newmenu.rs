//! 새로 만들기, ours.
//!
//! The shell's own NewMenu extension paints its items — it fills them in on
//! `WM_INITMENUPOPUP` and the icons are right — and then creates nothing:
//! it wants a shell-view site to select and rename what it made, and a
//! composition window is not one. So the entries are read from the registry
//! NewMenu itself reads (`HKCR\.<ext>\ShellNew`), the file is made here, and
//! the caller drops the new icon into inline rename the way explorer does.
//!
//! Every word the user sees comes out of shell32's string table rather than
//! being written here, so a Korean install says 폴더 / 새 폴더 and an English
//! one says Folder / New folder without a translation of our own.

use std::path::{Path, PathBuf};

use windows::Win32::Storage::FileSystem::SearchPathW;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::UI::Shell::{
    FOLDERID_Templates, KF_FLAG_DEFAULT, SHGetKnownFolderPath, SHLoadIndirectString, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

/// shell32 string ids: the folder entry's label, the "new thing" name template
/// and the one name that template does not cover.
const STR_FOLDER: u32 = 4131;
const STR_NEW_X: u32 = 30316;
const STR_NEW_FOLDER: u32 = 30396;

pub struct NewType {
    /// Menu text — the type's friendly name, e.g. 텍스트 문서.
    pub label: String,
    /// The name the file gets, where the type states one: `ItemName`, which
    /// already reads 새 비트맵 이미지 and is not a menu label.
    stem: Option<String>,
    /// Extension with its dot, empty for a folder.
    ext: String,
    how: How,
}

enum How {
    Folder,
    /// An empty file.
    Null,
    /// Copy this template — a full path, resolved and proven to exist while the
    /// menu was built.
    Template(PathBuf),
    /// Bytes to write, straight out of the registry.
    Data(Vec<u8>),
    /// Hand the job to a program with %1 as the path it should create: a
    /// wizard, not a file we could write. Expanded, and its exe found on disk.
    Command(String),
}

fn shell_str(id: u32) -> Option<String> {
    indirect(&format!("@shell32.dll,-{id}"))
}

/// `@dll,-id` is a resource reference, and the registry is full of them: it is
/// how a type carries a name in every language at once. Anything else is
/// already the string.
fn resolve(s: &str) -> Option<String> {
    let s = expand(s);
    if s.starts_with('@') { indirect(&s) } else { (!s.is_empty()).then_some(s) }
}

/// Half of ShellNew is written REG_EXPAND_SZ and the registry hands those back
/// unexpanded, so `%SystemRoot%\...` arrives as those literal characters.
fn expand(s: &str) -> String {
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let n = ExpandEnvironmentStringsW(PCWSTR(w.as_ptr()), None);
        if n == 0 {
            return s.to_string();
        }
        let mut buf = vec![0u16; n as usize];
        let n = ExpandEnvironmentStringsW(PCWSTR(w.as_ptr()), Some(&mut buf));
        match n {
            0 => s.to_string(),
            n => String::from_utf16_lossy(&buf[..n as usize - 1]),
        }
    }
}

fn indirect(src: &str) -> Option<String> {
    let src: Vec<u16> = src.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = [0u16; 260];
    unsafe { SHLoadIndirectString(PCWSTR(src.as_ptr()), &mut buf, None).ok()? };
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    (len > 0).then(|| String::from_utf16_lossy(&buf[..len]))
}

/// The whole submenu, in explorer's order: folder, shortcut, then the
/// registered types by name. Read once — a file type that registers itself
/// while the shell is up is not worth a registry walk per right-click.
pub fn types() -> &'static [NewType] {
    static TYPES: std::sync::OnceLock<Vec<NewType>> = std::sync::OnceLock::new();
    TYPES.get_or_init(build)
}

fn build() -> Vec<NewType> {
    // No 바로 가기 entry: its ShellNew names a Handler we cannot host, and the
    // classic stand-in, `rundll32 appwiz.cpl,NewLinkHere <path>`, is inert on
    // this Windows — it returns without a wizard and without a file, from the
    // command line as much as from here. An item that paints and does nothing
    // is the bug this module exists to remove.
    let mut out = vec![NewType {
        label: shell_str(STR_FOLDER).unwrap_or_else(|| "Folder".into()),
        stem: shell_str(STR_NEW_FOLDER),
        ext: String::new(),
        how: How::Folder,
    }];

    let hkcr = winreg::RegKey::predef(winreg::enums::HKEY_CLASSES_ROOT);
    let mut found: Vec<NewType> = Vec::new();
    for ext in hkcr.enum_keys().flatten() {
        if !ext.starts_with('.') {
            continue;
        }
        let Some(sn) = shellnew_key(&hkcr, &ext) else { continue };
        // Handler: a COM class that wants the shell view we do not have. An
        // entry we could only half-honour — 라이브러리 written as an empty
        // .library-ms is a broken file — is the bug we are here to fix, so it
        // is left out rather than painted.
        if sn.get_value::<String, _>("Handler").is_ok() {
            continue;
        }
        // Every arm proves it can deliver before the item is allowed to exist:
        // a template whose file was uninstalled, or a Command whose program is
        // not on this machine, would paint and then do nothing.
        let how = if let Ok(f) = sn.get_value::<String, _>("FileName") {
            let Some(src) = find_template(&f) else { continue };
            How::Template(src)
        } else if let Ok(d) = sn.get_raw_value("Data") {
            How::Data(d.bytes.to_vec())
        } else if let Ok(c) = sn.get_value::<String, _>("Command") {
            let c = expand(&c);
            match split_command(&c) {
                Some((exe, _)) if have_exe(&exe) => How::Command(c),
                _ => continue,
            }
        } else if sn.get_raw_value("NullFile").is_ok() {
            How::Null
        } else {
            continue;
        };
        // MenuText wins where a type sets one — that is what it is for — and
        // its & is an accelerator we do not underline.
        let label = sn
            .get_value::<String, _>("MenuText")
            .ok()
            .and_then(|s| resolve(&s))
            .or_else(|| friendly_name(&hkcr, &ext))
            .unwrap_or_else(|| format!("{} 파일", ext.trim_start_matches('.').to_uppercase()))
            .replace('&', "");
        let stem = sn.get_value::<String, _>("ItemName").ok().and_then(|s| resolve(&s));
        found.push(NewType { label, stem, ext, how });
    }
    found.sort_by(|a, b| a.label.cmp(&b.label));
    out.extend(found);
    out
}

/// A type's ShellNew is in one of two places: under the extension itself, or
/// under a ProgID subkey of it. `.zip` uses the second — `.zip\CompressedFolder`
/// — which is why 압축(ZIP) 폴더 was missing from a menu that only read the first.
fn shellnew_key(hkcr: &winreg::RegKey, ext: &str) -> Option<winreg::RegKey> {
    if let Ok(k) = hkcr.open_subkey(format!("{ext}\\ShellNew")) {
        return Some(k);
    }
    let key = hkcr.open_subkey(ext).ok()?;
    key.enum_keys().flatten().find_map(|sub| key.open_subkey(format!("{sub}\\ShellNew")).ok())
}

/// Where a bare `FileName` lives. `%SystemRoot%\ShellNew` is the one everybody
/// names; the Templates known folder is where an installer puts a per-user one,
/// and on this Windows the first is an empty directory.
fn template_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(root) = std::env::var_os("SystemRoot") {
        dirs.push(PathBuf::from(root).join("ShellNew"));
    }
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Templates, KF_FLAG_DEFAULT, None) {
            if let Ok(s) = p.to_string() {
                dirs.push(PathBuf::from(s));
            }
            CoTaskMemFree(Some(p.0 as *const _));
        }
    }
    dirs
}

fn find_template(name: &str) -> Option<PathBuf> {
    let name = PathBuf::from(expand(name));
    if name.is_absolute() {
        return name.is_file().then_some(name);
    }
    template_dirs().into_iter().map(|d| d.join(&name)).find(|c| c.is_file())
}

/// `"C:\Program Files\App\a.exe" /new "%1"` — a quoted program comes first and
/// the space inside it is not a separator, which is the whole reason this is
/// not `split_once(' ')`.
fn split_command(cmd: &str) -> Option<(String, String)> {
    let cmd = cmd.trim();
    match cmd.strip_prefix('"') {
        Some(rest) => {
            let (exe, args) = rest.split_once('"')?;
            Some((exe.to_string(), args.trim_start().to_string()))
        }
        None => Some(match cmd.split_once(' ') {
            Some((e, a)) => (e.to_string(), a.to_string()),
            None => (cmd.to_string(), String::new()),
        }),
    }
}

fn have_exe(exe: &str) -> bool {
    if exe.contains('\\') {
        return Path::new(exe).is_file();
    }
    let name: Vec<u16> = exe.encode_utf16().chain(std::iter::once(0)).collect();
    let ext: Vec<u16> = ".exe\0".encode_utf16().collect();
    unsafe {
        SearchPathW(None, PCWSTR(name.as_ptr()), PCWSTR(ext.as_ptr()), None, None) != 0
    }
}

/// The type's own name for itself: `.txt` → `txtfile` → 텍스트 문서.
/// FriendlyTypeName first — it is the localized one, and the default value is
/// whatever English the installer happened to write.
fn friendly_name(hkcr: &winreg::RegKey, ext: &str) -> Option<String> {
    let progid: String = hkcr.open_subkey(ext).ok()?.get_value("").ok()?;
    let key = hkcr.open_subkey(&progid).ok()?;
    key.get_value::<String, _>("FriendlyTypeName")
        .ok()
        .and_then(|s| resolve(&s))
        .or_else(|| key.get_value::<String, _>("").ok().and_then(|s| resolve(&s)))
}

/// Make one. Answers with the path if it exists by the time we return —
/// a wizard has not finished (or even started) when it does.
pub fn create(dir: &Path, t: &NewType) -> Option<PathBuf> {
    let stem = t.stem.clone().unwrap_or_else(|| {
        shell_str(STR_NEW_X)
            .map(|f| f.replace("%s", &t.label))
            .unwrap_or_else(|| format!("New {}", t.label))
    });
    let path = free_name(dir, &stem, &t.ext)?;
    match &t.how {
        How::Folder => std::fs::create_dir(&path).ok()?,
        How::Null => {
            std::fs::write(&path, []).ok()?;
        }
        How::Data(bytes) => std::fs::write(&path, bytes).ok()?,
        How::Template(src) => {
            std::fs::copy(src, &path).ok()?;
        }
        How::Command(cmd) => {
            run_command(cmd, &path);
            // The wizard writes the file when the user is done with it, if at
            // all; there is nothing to rename yet.
            return None;
        }
    }
    Some(path)
}

/// `새 폴더`, then `새 폴더 (2)` — explorer's numbering, and the reason the
/// name is built here rather than left to the caller.
fn free_name(dir: &Path, stem: &str, ext: &str) -> Option<PathBuf> {
    for n in 1..1000 {
        let name =
            if n == 1 { format!("{stem}{ext}") } else { format!("{stem} ({n}){ext}") };
        let path = dir.join(name);
        if !path.exists() {
            return Some(path);
        }
    }
    None
}

fn run_command(cmd: &str, path: &Path) {
    let cmd = cmd.replace("%1", &path.display().to_string());
    let Some((exe, args)) = split_command(&cmd) else { return };
    let exe: Vec<u16> = exe.encode_utf16().chain(std::iter::once(0)).collect();
    let args: Vec<u16> = args.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            None,
            PCWSTR::null(),
            PCWSTR(exe.as_ptr()),
            PCWSTR(args.as_ptr()),
            None,
            SW_SHOWNORMAL,
        );
    }
}
