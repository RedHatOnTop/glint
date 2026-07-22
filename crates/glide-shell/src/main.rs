//! glide-shell — resident shell replacement (design: docs/SHELL_DESIGN.md).
//!
//! Default run = M1 taskbar (alongside explorer; the appbar system stacks us
//! above the stock bar). `--spike-toasts` is the M4 gating spike, kept for
//! re-runs: UserNotificationListener passed PASS-POLLING on this box 0721.

mod autostart;
mod desktop;
mod flyout;
mod icons;
mod osd;
mod preview;
mod render;
mod secondary;
mod shellmenu;
mod spike_toasts;
mod startmenu;
mod status;
mod taskbar;
mod theme;
mod toasts;
mod tray;
mod wifi;
mod winkey;

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
        None | Some("--tray-claim") => {
            // --tray-claim: also register as Shell_TrayWnd. Racy while
            // explorer lives — meant for explorer-kill sessions (M2 gate).
            let claim = args.first().map(String::as_str) == Some("--tray-claim");
            unsafe {
                let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                    windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                );
            }
            // Shell duty (SHELL_DESIGN §6.5): the Run keys and Startup folders
            // only fire from here once Winlogon Shell= points at us; while
            // explorer is the shell this is a no-op, never a double launch.
            if autostart::is_system_shell() {
                std::thread::spawn(autostart::run_all);
            }
            taskbar::run(claim)
        }
        _ => {
            eprintln!(
                "usage: glide-shell [--tray-claim] [--spike-toasts [wait_secs]] [--autostart-list] [--autostart-selftest]"
            );
            Ok(())
        }
    }
}
