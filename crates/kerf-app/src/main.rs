#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod album_ui;
mod app;
mod assistant;
mod audio;
mod detect_ui;
mod history_panel;
mod icons;
mod jobs;
mod keymap;
mod markers_panel;
mod nav;
mod prefs;
mod recent;
mod recovery;
mod shortcuts_ui;
mod stats_panel;
mod timeline;

fn main() -> eframe::Result {
    let files: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("Kerf")
        // Matches the desktop file's name, so Wayland desktops show its icon and name.
        .with_app_id("io.github.anicke.Kerf")
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([640.0, 400.0])
        .with_drag_and_drop(true);
    if let Ok(icon) =
        eframe::icon_data::from_png_bytes(include_bytes!("../../../packaging/icons/kerf-256.png"))
    {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Kerf",
        options,
        Box::new(move |cc| Ok(Box::new(app::KerfApp::new(cc, files)))),
    )
}
