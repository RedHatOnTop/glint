//! M6 safety net (SHELL_DESIGN §7): --register/--unregister CLI, crash-counter
//! self-destruct, rescue-hotkey registry restore, panic log.
//!
//! Ladder recap: 1 in-process (panic log here; window death is covered by
//! process death) → 2 winlogon AutoRestartShell → 3 crash-counter
//! self-destruct → 4 rescue hotkeys (taskbar.rs WM_HOTKEY) → 5
//! RESTORE-SHELL.ps1 / Ctrl+Alt+Del → 6 rescue account.

use std::io::Write;

use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MessageBoxW, SW_SHOWNORMAL,
};
use windows::core::w;
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

const WINLOGON: &str = r"Software\Microsoft\Windows NT\CurrentVersion\Winlogon";
/// Self-destruct: this many crashes...
const CRASH_LIMIT: usize = 3;
/// ...within this window.
const CRASH_WINDOW_SECS: u64 = 600;

// ---- registry ---------------------------------------------------------------

fn winlogon_key(write: bool) -> std::io::Result<RegKey> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if write {
        hkcu.create_subkey(WINLOGON).map(|(k, _)| k)
    } else {
        hkcu.open_subkey_with_flags(WINLOGON, KEY_READ)
    }
}

pub fn query_shell() -> Option<String> {
    winlogon_key(false).ok()?.get_value::<String, _>("Shell").ok()
}

pub fn spawn_explorer() {
    unsafe {
        ShellExecuteW(None, w!("open"), w!("explorer.exe"), None, None, SW_SHOWNORMAL);
    }
}

/// Drop the HKCU Shell= override; next logon boots stock explorer.
pub fn restore_explorer_shell(also_spawn: bool) {
    if let Ok(k) = winlogon_key(true) {
        let _ = k.delete_value("Shell");
    }
    if also_spawn {
        spawn_explorer();
    }
}

// ---- CLI --------------------------------------------------------------------

fn confirm(prompt: &str) -> bool {
    print!("{prompt} — 계속하려면 YES 입력: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && line.trim() == "YES"
}

pub fn register() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let path = exe.to_string_lossy().to_string();
    println!("glide-shell 을 로그온 셸로 등록합니다.");
    println!("  HKCU\\...\\Winlogon Shell = {path}");
    println!("  적용: 다음 로그온부터. 복구: docs/SHELL_DESIGN.md §7 사다리 + RESTORE-SHELL.ps1.");
    if let Some(cur) = query_shell() {
        println!("  현재 값(덮어씀): {cur}");
    }
    if !confirm("셸 스왑") {
        println!("취소.");
        return Ok(());
    }
    winlogon_key(true)?.set_value("Shell", &path)?;
    println!("등록 완료. 현재 값: {}", query_shell().unwrap_or_default());
    Ok(())
}

pub fn unregister() -> anyhow::Result<()> {
    match query_shell() {
        None => println!("Shell= 값 없음 — 이미 stock explorer."),
        Some(cur) => {
            println!("HKCU Shell= 삭제 (현재: {cur}). 다음 로그온부터 stock explorer.");
            if !confirm("등록 해제") {
                println!("취소.");
                return Ok(());
            }
            winlogon_key(true)?.delete_value("Shell")?;
            println!("해제 완료.");
        }
    }
    Ok(())
}

// ---- crash counter ------------------------------------------------------------

fn state_dir() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("GLIDE_SHELL_STATE_DIR") {
        return p.into();
    }
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(base).join("glide-shell")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Call once at bar startup. A `running` sentinel left behind means the last
/// session never exited cleanly; three such starts inside the window is a
/// crash loop. `armed` (= we are the registered system shell) makes detection
/// actually fire the self-destruct; alongside-explorer runs only report.
pub fn crash_check_and_mark_running(armed: bool) -> bool {
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let sentinel = dir.join("session.state");
    let stamps_path = dir.join("crash_stamps.txt");

    let prev_crashed = std::fs::read_to_string(&sentinel)
        .map(|s| s.trim() == "running")
        .unwrap_or(false);
    let now = unix_now();
    let mut stamps: Vec<u64> = std::fs::read_to_string(&stamps_path)
        .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default();
    stamps.retain(|&t| now.saturating_sub(t) <= CRASH_WINDOW_SECS);
    if prev_crashed {
        stamps.push(now);
    }
    let _ = std::fs::write(
        &stamps_path,
        stamps.iter().map(|t| t.to_string()).collect::<Vec<_>>().join("\n"),
    );
    let _ = std::fs::write(&sentinel, "running");

    // Only a crash-triggered start may fire; stale stamps after a clean exit
    // must not re-trip the ladder.
    let looping = prev_crashed && stamps.len() >= CRASH_LIMIT;
    if looping && armed {
        restore_explorer_shell(true);
        unsafe {
            MessageBoxW(
                None,
                w!("glide-shell: 10분 내 3회 크래시 감지 — 셸 등록을 해제하고 explorer로 복귀했습니다.\n크래시 로그: %APPDATA%\\glide-shell\\crash.log"),
                w!("glide-shell 안전망"),
                MB_OK | MB_ICONWARNING | MB_SETFOREGROUND,
            );
        }
        std::process::exit(1);
    }
    looping
}

/// The message loop ended on purpose (quit/restart menu): next start is not a
/// crash.
pub fn mark_clean_exit() {
    let _ = std::fs::write(state_dir().join("session.state"), "clean");
}

/// Ladder rung 1: panics land in a log before AutoRestartShell respins us.
pub fn install_panic_log() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let p = state_dir().join("crash.log");
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "[{}] {info}", chrono::Local::now().format("%F %T"));
        }
        default(info);
    }));
}

/// Record something the user would want to know after the fact.
///
/// Once we are the shell there is no console behind stderr, so anything printed
/// there is gone — and the conditions worth reporting (a missing GPU, a service
/// that would not start) are exactly the ones nobody is watching a terminal
/// for. Same directory as `crash.log`, so one place holds the whole story.
pub fn note(msg: &str) {
    let p = state_dir().join("shell.log");
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = writeln!(f, "[{}] {msg}", chrono::Local::now().format("%F %T"));
    }
    eprintln!("glide-shell: {msg}");
}

// ---- selftest -----------------------------------------------------------------

/// M6 gate: 3-crash self-destruct simulation, against a temp state dir, never
/// touching the real registry (armed = false throughout).
pub fn selftest_crashloop() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("glide-shell-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    unsafe { std::env::set_var("GLIDE_SHELL_STATE_DIR", &dir) };

    let mut fails = 0;
    let mut check = |name: &str, got: bool, want: bool| {
        let ok = got == want;
        println!("{} {name}: looping={got} (want {want})", if ok { "PASS" } else { "FAIL" });
        if !ok {
            fails += 1;
        }
    };

    // Boot 1: clean history. Then three crashes (sentinel left as "running").
    check("boot 1 fresh", crash_check_and_mark_running(false), false);
    check("boot 2 after crash 1", crash_check_and_mark_running(false), false);
    check("boot 3 after crash 2", crash_check_and_mark_running(false), false);
    check("boot 4 after crash 3", crash_check_and_mark_running(false), true);

    // Clean exit clears the sentinel: stale stamps alone must not re-trip.
    mark_clean_exit();
    check("boot 5 after clean exit", crash_check_and_mark_running(false), false);

    let _ = std::fs::remove_dir_all(&dir);
    if fails > 0 {
        anyhow::bail!("{fails} selftest step(s) FAILED");
    }
    println!("crash-loop selftest: all PASS");
    Ok(())
}
