#![windows_subsystem = "windows"]

mod app;
mod cleaner;
mod disk_analyzer;
mod hasher;
mod large_files;
mod license;
mod scanner;
mod scheduler;
mod settings;
mod tray;
mod updater;

fn main() {
    // When launched by Task Scheduler the app auto-starts a scan of the last used folder.
    let auto_scan = std::env::args().any(|a| a == "--scheduled-scan");

    let icon = load_icon();
    let options = eframe::NativeOptions {
        drag_and_drop_support: true,
        initial_window_size: Some([1100.0, 700.0].into()),
        min_window_size: Some([800.0, 500.0].into()),
        icon_data: icon,
        ..Default::default()
    };

    let _ = eframe::run_native(
        "NeatDisk",
        options,
        Box::new(move |_cc| Box::new(app::App::new(auto_scan))),
    );
}

fn load_icon() -> Option<eframe::IconData> {
    let bytes = include_bytes!("../assets/icon.png");
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(eframe::IconData { rgba: img.into_raw(), width: w, height: h })
}
