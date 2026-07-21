//! glide-shell — resident shell replacement (design: docs/SHELL_DESIGN.md).
//!
//! Current state: M4 gating spike only. `--spike-toasts` verifies that
//! `UserNotificationListener` works from an unpackaged win32 process on this
//! machine — per SHELL_DESIGN §6.7 / §8, spike failure puts the whole swap
//! on hold, which is why this runs before M1.

mod spike_toasts;

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
        _ => {
            eprintln!("usage: glide-shell --spike-toasts [wait_secs]");
            Ok(())
        }
    }
}
