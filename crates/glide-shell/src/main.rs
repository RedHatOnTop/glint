//! glide-shell — resident shell replacement (design: docs/SHELL_DESIGN.md).
//!
//! Default run = M1 taskbar (alongside explorer; the appbar system stacks us
//! above the stock bar). `--spike-toasts` is the M4 gating spike, kept for
//! re-runs: UserNotificationListener passed PASS-POLLING on this box 0721.

mod actioncenter;
mod apps;
mod audiopolicy;
mod autostart;
mod clickaway;
mod config;
mod datetime;
mod desktop;
mod flyout;
mod icons;
mod menupopup;
mod osd;
mod power;
mod preview;
mod procs;
mod quicksettings;
mod render;
mod safety;
mod secondary;
mod services;
mod settings;
mod shellmenu;
mod spike_toasts;
mod startmenu;
mod status;
mod taskbar;
mod taskmgr;
mod theme;
mod toasts;
mod tray;
mod trayoverflow;
mod wifi;
mod winkey;
mod winsettings;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--spike-toasts") => {
            let wait_secs = args
                .get(1)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(30);
            spike_toasts::run(wait_secs)
        }
        Some("--autostart-list") => {
            autostart::list();
            Ok(())
        }
        Some("--autostart-selftest") => autostart::selftest(),
        Some("--register") => safety::register(),
        Some("--unregister") => safety::unregister(),
        Some("--selftest-crashloop") => safety::selftest_crashloop(),
        Some("--settings") => {
            // Standalone settings window (dev/verification; normally the bar
            // menu opens it in-process). Changes still land in settings.txt;
            // a running bar picks them up on its next WM_SETTINGS_CHANGED.
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                );
                let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                    windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                );
                theme::set_accent(config::load().accent);
                let dpi = windows::Win32::UI::HiDpi::GetDpiForSystem() as f32;
                let mut app = settings::SettingsApp::new(dpi)?;
                app.open(windows::Win32::Foundation::HWND(std::ptr::null_mut()));
                let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
                while windows::Win32::UI::WindowsAndMessaging::GetMessageW(&mut msg, None, 0, 0)
                    .into()
                {
                    let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                    windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
                }
            }
            Ok(())
        }
        Some("--taskmgr") => {
            // Standalone glide Task Manager (also how the settings 서비스 / 작업
            // 관리자 entries open it — a spawned `glide-shell --taskmgr`).
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                );
                let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                    windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                );
                theme::set_accent(config::load().accent);
                let dpi = windows::Win32::UI::HiDpi::GetDpiForSystem() as f32;
                let mut app = taskmgr::TaskManagerApp::new(dpi)?;
                app.open();
                let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
                while windows::Win32::UI::WindowsAndMessaging::GetMessageW(&mut msg, None, 0, 0)
                    .into()
                {
                    let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                    windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
                }
            }
            Ok(())
        }
        None | Some("--tray-claim") => {
            let system_shell = autostart::is_system_shell();
            // Winlogon starts the shell with no console to inherit, so the
            // loader allocates one and it sits on top of the desktop for the
            // whole session. It cannot go away at link time — --register reads
            // its confirmation off stdin — so drop it on this path only, and
            // hide rather than free: the handle stays valid, so every print
            // below still writes somewhere instead of failing.
            if system_shell {
                unsafe {
                    let con = windows::Win32::System::Console::GetConsoleWindow();
                    if !con.is_invalid() {
                        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                            con,
                            windows::Win32::UI::WindowsAndMessaging::SW_HIDE,
                        );
                    }
                }
            }
            unsafe {
                let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                    windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                );
            }
            // Registering as Shell_TrayWnd is not optional once Winlogon starts
            // us: SHAppBarMessage is served by whichever window holds that
            // class, so with no explorer and no claim there is nobody to honour
            // an appbar reservation — our own bar included — and real apps have
            // nowhere to put a tray icon. --tray-claim only exists to force it
            // in an explorer-kill session, where it is racy on purpose (M2).
            let claim = system_shell || args.first().map(String::as_str) == Some("--tray-claim");
            // Shell duty (SHELL_DESIGN §6.5): the Run keys and Startup folders
            // only fire from here once Winlogon Shell= points at us; while
            // explorer is the shell this is a no-op, never a double launch.
            if system_shell {
                std::thread::spawn(autostart::run_all);
            }
            safety::install_panic_log();
            // Ladder rung 3; alongside-explorer runs bookkeep but never fire.
            let _ = safety::crash_check_and_mark_running(system_shell);
            let r = taskbar::run(claim);
            safety::mark_clean_exit();
            r
        }
        _ => {
            eprintln!(
                "usage: glide-shell [--tray-claim] [--spike-toasts [wait_secs]] [--autostart-list] [--autostart-selftest]"
            );
            Ok(())
        }
    }
}
