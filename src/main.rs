#![windows_subsystem = "windows"]

mod app;
mod hasher;
mod license;
mod scanner;
mod settings;

fn main() {
    let icon = load_icon();
    let options = eframe::NativeOptions {
        drag_and_drop_support: true,
        initial_window_size: Some([1100.0, 700.0].into()),
        min_window_size: Some([800.0, 500.0].into()),
        icon_data: icon,
        ..Default::default()
    };

    let _ = eframe::run_native(
        "Duplicate Image Finder",
        options,
        Box::new(|_cc| Box::new(app::App::new())),
    );
}

fn load_icon() -> Option<eframe::IconData> {
    let bytes = include_bytes!("../assets/icon.png");
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(eframe::IconData { rgba: img.into_raw(), width: w, height: h })
}
