//! UserNotificationListener spike (SHELL_DESIGN §6.7).
//!
//! Verdict this must produce, on this box, unpackaged:
//!   1. Can we get the listener + Allowed access status?
//!   2. Do we see existing toasts (app name + text)?
//!   3. Does NotificationChanged fire for a toast raised while listening?
//!
//! The API is documented against packaged (identity-carrying) apps; unpackaged
//! behavior varies by build, which is exactly what the spike settles.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::Management::{
    UserNotificationListener, UserNotificationListenerAccessStatus,
};
use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds, UserNotification};

static CHANGE_EVENTS: AtomicU32 = AtomicU32::new(0);

pub fn run(wait_secs: u64) -> anyhow::Result<()> {
    println!("[spike] getting UserNotificationListener::Current()...");
    let listener = match UserNotificationListener::Current() {
        Ok(l) => l,
        Err(e) => {
            println!("VERDICT: FAIL — Current() error: {e:?}");
            return Ok(());
        }
    };
    println!("[spike] listener obtained");

    let access = match listener.RequestAccessAsync() {
        Ok(op) => match op.join() {
            Ok(a) => a,
            Err(e) => {
                println!("VERDICT: FAIL — RequestAccessAsync.join() error: {e:?}");
                return Ok(());
            }
        },
        Err(e) => {
            println!("VERDICT: FAIL — RequestAccessAsync() error: {e:?}");
            return Ok(());
        }
    };
    println!("[spike] access status = {access:?}");
    if access != UserNotificationListenerAccessStatus::Allowed {
        println!(
            "VERDICT: DENIED — enable in ms-settings:privacy-notifications \
             (CapabilityAccessManager userNotificationListener)"
        );
        return Ok(());
    }

    match listener.GetNotificationsAsync(NotificationKinds::Toast) {
        Ok(op) => match op.join() {
            Ok(existing) => {
                println!("[spike] existing toasts in action center: {}", existing.Size()?);
                for n in &existing {
                    print_notification(&n);
                }
            }
            Err(e) => println!("[spike] GetNotificationsAsync.join() error: {e:?}"),
        },
        Err(e) => println!("[spike] GetNotificationsAsync() error: {e:?}"),
    }

    let handler = TypedEventHandler::new(|_sender, _args| {
        let n = CHANGE_EVENTS.fetch_add(1, Ordering::SeqCst) + 1;
        println!("[spike] NotificationChanged fired (#{n})");
        Ok(())
    });
    let token = match listener.NotificationChanged(&handler) {
        Ok(t) => {
            println!("[spike] subscribed to NotificationChanged; waiting {wait_secs}s for a live toast...");
            Some(t)
        }
        Err(e) => {
            println!("[spike] NotificationChanged subscribe error: {e:?} — falling back to polling");
            None
        }
    };

    let start = Instant::now();
    let baseline = count_toasts(&listener);
    let mut polled_new = false;
    while start.elapsed() < Duration::from_secs(wait_secs) {
        std::thread::sleep(Duration::from_millis(500));
        if CHANGE_EVENTS.load(Ordering::SeqCst) > 0 {
            break;
        }
        if count_toasts(&listener) > baseline {
            polled_new = true;
            break;
        }
    }

    println!("[spike] final action center contents:");
    match listener.GetNotificationsAsync(NotificationKinds::Toast) {
        Ok(op) => {
            if let Ok(after) = op.join() {
                for n in &after {
                    print_notification(&n);
                }
            }
        }
        Err(e) => println!("[spike] re-enumerate error: {e:?}"),
    }

    if let Some(t) = token {
        let _ = listener.RemoveNotificationChanged(t);
    }

    let events = CHANGE_EVENTS.load(Ordering::SeqCst);
    if events > 0 {
        println!("VERDICT: PASS — event-driven ({events} NotificationChanged events)");
    } else if polled_new {
        println!("VERDICT: PASS-POLLING — no events, but polling saw a new toast (usable, degraded)");
    } else {
        println!("VERDICT: NO-TOAST-SEEN — access OK but nothing arrived in the window (inconclusive; re-run with a live toast)");
    }
    Ok(())
}

fn count_toasts(listener: &UserNotificationListener) -> u32 {
    listener
        .GetNotificationsAsync(NotificationKinds::Toast)
        .and_then(|op| op.join())
        .and_then(|v| v.Size())
        .unwrap_or(0)
}

fn print_notification(n: &UserNotification) {
    let app = n
        .AppInfo()
        .and_then(|a| a.DisplayInfo())
        .and_then(|d| d.DisplayName())
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "<unknown app>".into());
    let mut texts: Vec<String> = Vec::new();
    if let Ok(visual) = n.Notification().and_then(|c| c.Visual()) {
        if let Ok(binding) =
            KnownNotificationBindings::ToastGeneric().and_then(|b| visual.GetBinding(&b))
        {
            if let Ok(elements) = binding.GetTextElements() {
                for t in &elements {
                    if let Ok(s) = t.Text() {
                        texts.push(s.to_string());
                    }
                }
            }
        }
    }
    println!("  - [{app}] {}", texts.join(" | "));
}
