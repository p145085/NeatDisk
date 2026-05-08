mod app;
mod hasher;
mod license;
mod scanner;
mod settings;

fn main() {
    let options = eframe::NativeOptions {
        drag_and_drop_support: true,
        initial_window_size: Some([1100.0, 700.0].into()),
        min_window_size: Some([800.0, 500.0].into()),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "Duplicate Image Finder",
        options,
        Box::new(|_cc| Box::new(app::App::new())),
    );
}
