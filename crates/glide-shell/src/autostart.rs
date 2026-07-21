//! M3 autostart executor (SHELL_DESIGN §6.5). When glide-shell is the
//! registered shell, explorer never runs, so the Run/RunOnce keys and the
//! Startup folders are our job — Everything lives in HKCU Run, and
//! glint/glide search dies without it.
//!
//! Honesty rule: entries the user disabled in Task Manager live on in the
//! Run keys and are only vetoed by the StartupApproved registry — skipping
//! that check would resurrect every startup app the user killed.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Threading::{
    CreateProcessW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, PWSTR, w};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY};
use winreg::types::FromRegValue;

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_ONCE: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

#[derive(Clone, Copy, PartialEq)]
pub enum Source {
    HkcuRun,
    HklmRun,
    HklmRun32,
    HkcuRunOnce,
    HklmRunOnce,
    UserStartup,
    CommonStartup,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::HkcuRun => "HKCU Run",
            Source::HklmRun => "HKLM Run",
            Source::HklmRun32 => "HKLM Run (32)",
            Source::HkcuRunOnce => "HKCU RunOnce",
            Source::HklmRunOnce => "HKLM RunOnce",
            Source::UserStartup => "Startup (user)",
            Source::CommonStartup => "Startup (common)",
        }
    }
}

pub struct Entry {
    pub name: String,
    /// Registry sources: the raw command line. Folder sources: the file path.
    pub command: String,
    pub source: Source,
    /// StartupApproved verdict; RunOnce has no approval mechanism.
    pub enabled: bool,
}

/// StartupApproved values are 12-byte blobs; an even first byte means
/// enabled, odd means disabled (bytes 4-11 hold the disable timestamp).
fn approved_map(root: winreg::HKEY, subkey: &str) -> HashMap<String, bool> {
    let mut map = HashMap::new();
    if let Ok(k) = RegKey::predef(root).open_subkey(format!(r"{APPROVED}\{subkey}")) {
        for (name, value) in k.enum_values().flatten() {
            let enabled = value.bytes.first().is_none_or(|b| b & 1 == 0);
            map.insert(name.to_ascii_lowercase(), enabled);
        }
    }
    map
}

fn reg_entries(
    root: winreg::HKEY,
    path: &str,
    flags: u32,
    source: Source,
    approved: Option<&HashMap<String, bool>>,
) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(key) = RegKey::predef(root).open_subkey_with_flags(path, KEY_READ | flags) else {
        return out;
    };
    for (name, value) in key.enum_values().flatten() {
        let Ok(command) = String::from_reg_value(&value) else { continue };
        if command.trim().is_empty() {
            continue;
        }
        let enabled = approved
            .and_then(|m| m.get(&name.to_ascii_lowercase()).copied())
            .unwrap_or(true);
        out.push(Entry { name, command, source, enabled });
    }
    out
}

fn folder_entries(dir: Option<PathBuf>, source: Source, approved: &HashMap<String, bool>) -> Vec<Entry> {
    let mut out = Vec::new();
    let Some(dir) = dir else { return out };
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for f in rd.flatten() {
        let path = f.path();
        if !path.is_file() {
            continue;
        }
        let name = f.file_name().to_string_lossy().into_owned();
        if name.eq_ignore_ascii_case("desktop.ini") {
            continue;
        }
        let enabled = approved
            .get(&name.to_ascii_lowercase())
            .copied()
            .unwrap_or(true);
        out.push(Entry {
            name,
            command: path.to_string_lossy().into_owned(),
            source,
            enabled,
        });
    }
    out
}

pub fn enumerate() -> Vec<Entry> {
    let hkcu_approved = approved_map(HKEY_CURRENT_USER, "Run");
    let hklm_approved = approved_map(HKEY_LOCAL_MACHINE, "Run");
    let hklm_approved32 = approved_map(HKEY_LOCAL_MACHINE, "Run32");
    let user_folder_approved = approved_map(HKEY_CURRENT_USER, "StartupFolder");
    let common_folder_approved = approved_map(HKEY_LOCAL_MACHINE, "StartupFolder");

    let startup_tail = r"Microsoft\Windows\Start Menu\Programs\Startup";
    let user_startup = std::env::var("APPDATA").ok().map(|p| PathBuf::from(p).join(startup_tail));
    let common_startup =
        std::env::var("ProgramData").ok().map(|p| PathBuf::from(p).join(startup_tail));

    let mut out = Vec::new();
    out.extend(reg_entries(HKEY_CURRENT_USER, RUN, 0, Source::HkcuRun, Some(&hkcu_approved)));
    out.extend(reg_entries(HKEY_LOCAL_MACHINE, RUN, 0, Source::HklmRun, Some(&hklm_approved)));
    out.extend(reg_entries(
        HKEY_LOCAL_MACHINE,
        RUN,
        KEY_WOW64_32KEY,
        Source::HklmRun32,
        Some(&hklm_approved32),
    ));
    // RunOnce: no StartupApproved veto exists; always considered enabled.
    out.extend(reg_entries(HKEY_CURRENT_USER, RUN_ONCE, 0, Source::HkcuRunOnce, None));
    out.extend(reg_entries(HKEY_LOCAL_MACHINE, RUN_ONCE, 0, Source::HklmRunOnce, None));
    out.extend(folder_entries(user_startup, Source::UserStartup, &user_folder_approved));
    out.extend(folder_entries(common_startup, Source::CommonStartup, &common_folder_approved));
    out
}

fn expand_env(s: &str) -> String {
    unsafe {
        let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        let n = ExpandEnvironmentStringsW(PCWSTR(wide.as_ptr()), None);
        if n == 0 {
            return s.to_string();
        }
        let mut buf = vec![0u16; n as usize + 1];
        let n = ExpandEnvironmentStringsW(PCWSTR(wide.as_ptr()), Some(&mut buf));
        if n == 0 {
            return s.to_string();
        }
        String::from_utf16_lossy(&buf[..n.saturating_sub(1) as usize])
    }
}

/// Run one entry the way explorer would: registry command lines through
/// CreateProcess (they carry their own arguments), folder items through
/// ShellExecute (mostly .lnk).
pub fn execute(e: &Entry) -> bool {
    match e.source {
        Source::UserStartup | Source::CommonStartup => unsafe {
            let wide: Vec<u16> = e.command.encode_utf16().chain(std::iter::once(0)).collect();
            let h = ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            );
            h.0 as usize > 32
        },
        _ => unsafe {
            let expanded = expand_env(&e.command);
            let mut cmd: Vec<u16> = expanded.encode_utf16().chain(std::iter::once(0)).collect();
            let si = STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                ..Default::default()
            };
            let mut pi = PROCESS_INFORMATION::default();
            let ok = CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(cmd.as_mut_ptr())),
                None,
                None,
                false,
                PROCESS_CREATION_FLAGS(0),
                None,
                PCWSTR::null(),
                &si,
                &mut pi,
            )
            .is_ok();
            if ok {
                let _ = CloseHandle(pi.hProcess);
                let _ = CloseHandle(pi.hThread);
            }
            ok
        },
    }
}

/// The real shell duty: run everything enabled. RunOnce values are deleted
/// before launching, matching Windows' contract for names without '!'.
pub fn run_all() {
    for e in enumerate() {
        if !e.enabled {
            continue;
        }
        match e.source {
            Source::HkcuRunOnce => delete_runonce(HKEY_CURRENT_USER, &e.name),
            Source::HklmRunOnce => delete_runonce(HKEY_LOCAL_MACHINE, &e.name),
            _ => {}
        }
        let ok = execute(&e);
        eprintln!(
            "autostart: [{}] {} — {}",
            e.source.label(),
            e.name,
            if ok { "launched" } else { "FAILED" }
        );
    }
}

fn delete_runonce(root: winreg::HKEY, name: &str) {
    if let Ok(k) = RegKey::predef(root)
        .open_subkey_with_flags(RUN_ONCE, winreg::enums::KEY_SET_VALUE)
    {
        let _ = k.delete_value(name);
    }
}

/// True only when Winlogon Shell= points at us — the guard that keeps the
/// executor from double-launching everything while explorer is still the
/// shell. HKCU overrides the HKLM default.
pub fn is_system_shell() -> bool {
    let read = |root| -> Option<String> {
        RegKey::predef(root)
            .open_subkey(r"Software\Microsoft\Windows NT\CurrentVersion\Winlogon")
            .and_then(|k| k.get_value::<String, _>("Shell"))
            .ok()
    };
    let shell = read(HKEY_CURRENT_USER)
        .or_else(|| read(HKEY_LOCAL_MACHINE))
        .unwrap_or_else(|| "explorer.exe".to_string());
    shell.to_ascii_lowercase().contains("glide-shell")
}

/// `--autostart-list`: dry run, prints the verdict for every entry.
pub fn list() {
    println!(
        "system shell: {} (executor {} at startup)\n",
        if is_system_shell() { "glide-shell" } else { "explorer" },
        if is_system_shell() { "RUNS" } else { "stays off" },
    );
    for e in enumerate() {
        println!(
            "{:9} {:17} {:40} {}",
            if e.enabled { "enabled" } else { "DISABLED" },
            e.source.label(),
            e.name,
            e.command
        );
    }
}

/// `--autostart-selftest`: plants a synthetic HKCU Run value, checks it is
/// enumerated as enabled, executes it through the real path, verifies the
/// marker file, and cleans up after itself (in-process winreg — no shell
/// commands involved).
pub fn selftest() -> anyhow::Result<()> {
    const NAME: &str = "glide_autostart_selftest";
    let marker = std::env::temp_dir().join("glide_autostart_selftest.txt");
    let _ = std::fs::remove_file(&marker);

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) = hkcu.create_subkey(RUN)?;
    let cmd = format!("cmd /c echo ok> \"{}\"", marker.display());
    run.set_value(NAME, &cmd)?;

    let result = (|| -> anyhow::Result<()> {
        let entries = enumerate();
        let e = entries
            .iter()
            .find(|e| e.name == NAME && e.source == Source::HkcuRun)
            .context("selftest entry missing from enumerate()")?;
        anyhow::ensure!(e.enabled, "selftest entry reported disabled");
        anyhow::ensure!(execute(e), "execute() failed");
        for _ in 0..50 {
            if marker.exists() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        anyhow::bail!("marker file never appeared");
    })();

    let _ = run.delete_value(NAME);
    let _ = std::fs::remove_file(&marker);
    result?;
    println!("autostart selftest: PASS (plant → enumerate → execute → marker → cleanup)");
    Ok(())
}
