// glide — dual-pane Win11-native file manager (glint's sibling).
#![windows_subsystem = "windows"]

mod app;
mod ipc;
mod ops;
mod pane;
mod preview;
mod register;
mod shellmenu;
mod sidebar;
mod theme;

fn main() -> eframe::Result {
    env_logger::init();
    // Single instance: hand our args to a running glide (they become tabs
    // there) and bow out, instead of spawning another full process.
    let args: Vec<std::path::PathBuf> = std::env::args().skip(1).map(Into::into).collect();
    if ipc::send_to_existing(&args) {
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1150.0, 720.0])
            .with_min_inner_size([760.0, 420.0])
            .with_transparent(true) // Mica shows through the clear color
            .with_title("Glide"),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Glide",
        options,
        Box::new(|cc| {
            glint_core::platform::apply_win11_chrome(cc, glint_core::platform::Backdrop::Mica);
            glint_core::platform::install_fonts(&cc.egui_ctx);
            theme::install(&cc.egui_ctx);
            Ok(Box::new(app::GlideApp::new(cc)))
        }),
    )
}
