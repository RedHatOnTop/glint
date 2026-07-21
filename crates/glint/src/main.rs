// glint — Windows 11 style unified launcher (files / apps / web)
#![windows_subsystem = "windows"]

mod app;

use glint_core::platform;

use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};

const WIN_W: f32 = 680.0;
const WIN_H: f32 = 520.0;

fn main() -> eframe::Result {
    env_logger::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([WIN_W, WIN_H])
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_resizable(false)
            .with_taskbar(false)
            .with_visible(true),
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "glint",
        options,
        Box::new(|cc| {
            use std::sync::atomic::Ordering;
            platform::apply_win11_chrome(cc, platform::Backdrop::Acrylic);
            platform::install_fonts(&cc.egui_ctx);
            app::WINDOW_HWND.store(platform::hwnd_isize(cc), Ordering::SeqCst);

            // Global hotkey: Alt+Space toggles the palette.
            let manager = GlobalHotKeyManager::new().expect("hotkey manager");
            let hotkey = HotKey::new(Some(Modifiers::ALT), Code::Space);
            manager.register(hotkey).expect("register Alt+Space");

            // Toggle window visibility HERE on the handler thread — when the
            // window is hidden, eframe stops calling update(), so the flag
            // alone would never be seen.
            let ctx = cc.egui_ctx.clone();
            GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
                if e.state() == HotKeyState::Pressed {
                    let hwnd = app::WINDOW_HWND.load(Ordering::SeqCst);
                    let show = !app::VISIBLE.load(Ordering::SeqCst);
                    platform::set_window_visible(hwnd, show);
                    app::VISIBLE.store(show, Ordering::SeqCst);
                    app::HOTKEY_PRESSED.store(true, Ordering::SeqCst);
                    ctx.request_repaint();
                }
            }));

            // glide-shell's Win-key hook toggles us over a named pipe —
            // identical effect to the hotkey path above.
            let ctx = cc.egui_ctx.clone();
            glint_core::toggle_pipe::listen(move || {
                let hwnd = app::WINDOW_HWND.load(Ordering::SeqCst);
                let show = !app::VISIBLE.load(Ordering::SeqCst);
                platform::set_window_visible(hwnd, show);
                app::VISIBLE.store(show, Ordering::SeqCst);
                app::HOTKEY_PRESSED.store(true, Ordering::SeqCst);
                ctx.request_repaint();
            });

            Ok(Box::new(app::GlintApp::new(cc, manager)))
        }),
    )
}
