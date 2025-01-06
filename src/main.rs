use egui::{CentralPanel, ProgressBar, Vec2, ScrollArea, RichText, TextureOptions};
use eframe::egui;
use walkdir::WalkDir;
use std::{
    collections::HashMap,
    fs,
};
use std::path::PathBuf;
use image::imageops::FilterType;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use rfd::FileDialog;
use std::path::Path;
use clipboard::{ClipboardContext, ClipboardProvider};

// Add this new struct to track scan progress
#[derive(Clone)]
struct ScanProgress {
    current: usize,
    total: usize,
}

#[derive(Clone)]
enum ScanMessage {
    Progress(ScanProgress),
    Complete(HashMap<String, Vec<String>>),
}

#[derive(Default)]
pub struct MyApp {
    path: String,
    duplicates: HashMap<String, Vec<String>>,
    texture_handles: HashMap<String, egui::TextureHandle>,
    scan_complete: bool,
    is_scanning: bool,
    progress_rx: Option<Receiver<ScanMessage>>,
    current_progress: Option<ScanProgress>,
    include_hidden: bool,
}

impl MyApp {
    fn new() -> Self {
        Self {
            path: String::new(),
            duplicates: HashMap::new(),
            texture_handles: HashMap::new(),
            scan_complete: false,
            is_scanning: false,
            progress_rx: None,
            current_progress: None,
            include_hidden: false,
        }
    }

    fn scan_directory(&mut self) {
        let path = self.path.clone();
        let include_hidden = self.include_hidden;
        self.duplicates.clear();
        self.texture_handles.clear();
        self.scan_complete = false;
        self.is_scanning = true;
        
        let (tx, rx) = mpsc::channel();
        self.progress_rx = Some(rx);
        
        thread::spawn(move || {
            println!("\nStarting scan in directory: {}", path);
            println!("Include hidden folders: {}", include_hidden);
            
            // Collect all files first
            let files: Vec<_> = WalkDir::new(&path)
                .into_iter()
                .filter_map(|e| {
                    let entry = e.ok()?;
                    let path_str = entry.path().to_string_lossy().to_string();
                    
                    // Check if it's a file
                    let is_file = entry.file_type().is_file();
                    if !is_file {
                        println!("Skipping non-file: {}", path_str);
                        return None;
                    }
                    
                    // Check if path is hidden
                    let is_hidden = entry.path().components().any(|comp| {
                        let name = comp.as_os_str().to_string_lossy();
                        name.starts_with('.') || name.starts_with("$")
                    });
                    
                    if is_hidden && !include_hidden {
                        println!("Skipping hidden path: {}", path_str);
                        return None;
                    }
                    
                    // Check file extension and try to open the file
                    let extension = match entry.path().extension() {
                        Some(ext) => ext.to_str().unwrap_or("").to_lowercase(),
                        None => {
                            println!("Skipping file without extension: {}", path_str);
                            return None;
                        }
                    };
                    
                    let is_image = matches!(
                        extension.as_str(),
                        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp"
                    );
                    
                    if !is_image {
                        println!("Skipping non-image file ({}): {}", extension, path_str);
                        return None;
                    }

                    // Try to open the file to verify it's accessible
                    match std::fs::metadata(entry.path()) {
                        Ok(metadata) => {
                            if metadata.len() == 0 {
                                println!("Skipping empty file: {}", path_str);
                                return None;
                            }
                            println!("Found valid image: {} (size: {} bytes)", path_str, metadata.len());
                        }
                        Err(e) => {
                            println!("Error accessing file {}: {}", path_str, e);
                            return None;
                        }
                    }
                    
                    Some(path_str)
                })
                .collect();

            let total_files = files.len();
            println!("\nFound {} image files to process", total_files);
            
            if total_files == 0 {
                tx.send(ScanMessage::Complete(HashMap::new())).ok();
                return;
            }

            tx.send(ScanMessage::Progress(ScanProgress {
                current: 0,
                total: total_files,
            })).ok();

            // Process files and collect hashes
            let mut all_files: HashMap<String, Vec<String>> = HashMap::new();
            
            for (i, file) in files.iter().enumerate() {
                println!("Processing {}/{}: {}", i + 1, total_files, file);
                
                if let Some(hash) = compute_image_hash(file) {
                    println!("Adding file to hash group {} -> {}", hash, file);
                    all_files.entry(hash).or_insert_with(Vec::new).push(file.clone());
                }
                
                tx.send(ScanMessage::Progress(ScanProgress {
                    current: i + 1,
                    total: total_files,
                })).ok();
            }

            println!("\nBefore filtering - All hash groups:");
            for (hash, paths) in &all_files {
                println!("Hash {}: {} files", hash, paths.len());
                for path in paths {
                    println!("  {}", path);
                }
            }

            // Filter out non-duplicates
            all_files.retain(|_, paths| paths.len() > 1);
            
            println!("\nAfter filtering - Duplicate groups:");
            for (hash, paths) in &all_files {
                println!("Hash {}: {} files", hash, paths.len());
                for path in paths {
                    println!("  {}", path);
                }
            }

            tx.send(ScanMessage::Complete(all_files)).ok();
        });
    }

    fn delete_image(&mut self, path: &str) {
        if let Err(e) = fs::remove_file(path) {
            eprintln!("Failed to delete file {}: {}", path, e);
        }
        self.texture_handles.remove(path);
        
        // Remove the path from duplicates
        for paths in self.duplicates.values_mut() {
            paths.retain(|p| p != path);
        }
        // Remove groups that no longer have duplicates
        self.duplicates.retain(|_, paths| paths.len() > 1);
    }

    fn rename_image(&mut self, path: &str) {
        let path_buf = PathBuf::from(path);
        if let (Some(parent), Some(filename)) = (path_buf.parent(), path_buf.file_name()) {
            let filename = filename.to_string_lossy();
            let new_name = format!("{}_copy{}", 
                filename.split('.').next().unwrap_or("file"),
                path_buf.extension().map(|ext| format!(".{}", ext.to_string_lossy()))
                    .unwrap_or_default()
            );
            let new_path = parent.join(new_name);
            
            if let Err(e) = fs::rename(&path_buf, &new_path) {
                eprintln!("Failed to rename file {}: {}", path, e);
            } else {
                self.texture_handles.remove(path);
                
                // Update path in duplicates
                for paths in self.duplicates.values_mut() {
                    if let Some(pos) = paths.iter().position(|p| p == path) {
                        paths[pos] = new_path.to_string_lossy().into_owned();
                    }
                }
            }
        }
    }

    fn load_texture(&mut self, ctx: &egui::Context, path: &str) -> Option<egui::TextureHandle> {
        if let Some(handle) = self.texture_handles.get(path) {
            return Some(handle.clone());
        }

        let image = image::open(path).ok()?;
        
        // Resize image if it's too large
        let max_size = 800;
        let image = if image.width() > max_size as u32 || image.height() > max_size as u32 {
            image.resize(
                max_size as u32,
                max_size as u32,
                FilterType::Triangle
            )
        } else {
            image
        };

        let size = [image.width() as _, image.height() as _];
        let image_buffer = image.to_rgba8();
        let pixels = image_buffer.as_flat_samples();
        
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            size,
            pixels.as_slice(),
        );
        
        let handle = ctx.load_texture(
            path,
            color_image,
            TextureOptions::default(),
        );
        
        self.texture_handles.insert(path.to_string(), handle.clone());
        Some(handle)
    }

    fn format_path_with_filename(&self, path: &str) -> RichText {
        let path = Path::new(path);
        if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
            if let Some(parent) = path.parent().and_then(|p| p.to_str()) {
                // Create the directory path with gray color
                let dir_text = RichText::new(format!("{}/", parent))
                    .monospace()
                    .color(egui::Color32::GRAY);
                
                // Create the filename with orange color and bold
                let filename_text = RichText::new(format!("\n↳ {}", filename))
                    .monospace()
                    .color(egui::Color32::from_rgb(255, 140, 0))
                    .strong();
                
                // Combine them into a single string
                RichText::new(format!("{}{}", dir_text.text(), filename_text.text()))
            } else {
                // Just filename, no directory
                RichText::new(filename)
                    .monospace()
                    .color(egui::Color32::from_rgb(255, 140, 0))
                    .strong()
            }
        } else {
            // Fallback for invalid paths
            RichText::new(path.to_string_lossy().to_string())
                .monospace()
                .color(egui::Color32::GRAY)
        }
    }

    fn copy_to_clipboard(&self, text: &str) {
        if let Ok(mut clipboard) = ClipboardContext::new() {
            if let Err(e) = clipboard.set_contents(text.to_owned()) {
                eprintln!("Failed to copy to clipboard: {}", e);
            }
        }
    }
}

fn compute_image_hash(image_path: &str) -> Option<String> {
    println!("Computing hash for: {}", image_path);
    
    let img = match image::open(image_path) {
        Ok(img) => {
            println!("Successfully opened image: {} ({}x{})", image_path, img.width(), img.height());
            img
        }
        Err(e) => {
            println!("Failed to open image {}: {}", image_path, e);
            return None;
        }
    };
    
    // Convert to grayscale and resize to a very small size to reduce detail
    let small_img = img.resize_exact(8, 8, FilterType::Nearest).to_luma8();
    let pixels = small_img.as_raw();
    
    // Calculate average pixel value
    let avg: u8 = (pixels.iter().map(|&p| p as u32).sum::<u32>() / pixels.len() as u32) as u8;
    
    // Create a simpler hash with less precision
    let mut hash = String::with_capacity(64);
    for &pixel in pixels {
        // Add some tolerance around the average
        hash.push(if (pixel as i16 - avg as i16).abs() > 5 { '1' } else { '0' });
    }
    
    println!("Generated hash for {}: {}", image_path, hash);
    Some(hash)
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Handle messages in a way that avoids the borrow checker issue
        let mut completed = false;
        let mut new_results = None;
        
        if let Some(rx) = &self.progress_rx {
            while let Ok(message) = rx.try_recv() {
                match message {
                    ScanMessage::Progress(progress) => {
                        self.current_progress = Some(progress);
                    }
                    ScanMessage::Complete(results) => {
                        new_results = Some(results);
                        completed = true;
                    }
                }
            }
        }
        
        // Apply completion results after the loop
        if completed {
            if let Some(results) = new_results {
                // Process results in chunks to prevent UI freeze
                ctx.request_repaint(); // Ensure UI stays responsive
                self.duplicates = results;
                self.is_scanning = false;
                self.scan_complete = true;
                self.progress_rx = None;
                
                // Add a small delay to allow UI to update
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Path:");
                ui.text_edit_singleline(&mut self.path);
                
                if ui.button("Browse...").clicked() {
                    if let Some(path) = FileDialog::new()
                        .set_title("Select Directory to Scan")
                        .pick_folder() {
                            self.path = path.to_string_lossy().to_string();
                    }
                }
                
                ui.checkbox(&mut self.include_hidden, "Include Hidden Folders");
                
                if ui.button("Scan Directory").clicked() && !self.path.is_empty() && !self.is_scanning {
                    self.scan_directory();
                }
            });

            if self.is_scanning {
                ui.add_space(10.0);
                if let Some(progress) = &self.current_progress {
                    let progress_fraction = progress.current as f32 / progress.total as f32;
                    if progress_fraction >= 1.0 {
                        ui.add(ProgressBar::new(1.0).text("Processing results..."));
                    } else {
                        ui.add(
                            ProgressBar::new(progress_fraction)
                                .text(format!("Scanning... {}/{}", progress.current, progress.total))
                        );
                    }
                } else {
                    ui.add(ProgressBar::new(0.0).text("Preparing scan..."));
                }
            }

            if self.scan_complete && !self.is_scanning {
                if self.duplicates.is_empty() {
                    ui.add_space(10.0);
                    ui.label("No duplicates found.");
                    return;
                }

                // Clone the data we need for rendering
                let duplicates_data: Vec<(String, Vec<String>)> = self.duplicates
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();

                ScrollArea::vertical().show(ui, |ui| {
                    for (_, paths) in duplicates_data {
                        ui.add_space(10.0);
                        ui.group(|ui| {
                            let root_image = &paths[0];
                            ui.horizontal(|ui| {
                                // Root image on the left
                                ui.vertical(|ui| {
                                    if let Some(texture) = self.load_texture(ctx, root_image) {
                                        let size = texture.size_vec2();
                                        let aspect = size.x / size.y;
                                        let height = 200.0;
                                        ui.image(texture.id(), Vec2::new(height * aspect, height));
                                    }
                                    
                                    // Add buttons for root image
                                    ui.horizontal(|ui| {
                                        if ui.button(RichText::new("Keep").size(14.0)).clicked() {
                                            for other_path in &paths {
                                                if other_path != root_image {
                                                    self.delete_image(other_path);
                                                }
                                            }
                                        }
                                        if ui.button(RichText::new("Rename").size(14.0)).clicked() {
                                            self.rename_image(root_image);
                                        }
                                        if ui.button(RichText::new("Delete").size(14.0)).clicked() {
                                            self.delete_image(root_image);
                                        }
                                    });
                                    
                                    ui.add_space(5.0);
                                    // Constrain the width of the path display
                                    ui.with_layout(egui::Layout::left_to_right(egui::Align::LEFT).with_cross_justify(true), |ui| {
                                        let available_width = ui.available_width().min(300.0);
                                        ui.set_width(available_width);
                                        if ui.add(egui::Label::new(self.format_path_with_filename(root_image))
                                            .wrap(true)
                                            .sense(egui::Sense::click()))
                                            .clicked() {
                                            self.copy_to_clipboard(root_image);
                                            ui.output_mut(|o| o.copied_text = root_image.to_string());
                                        }
                                    });
                                });

                                ui.add_space(20.0);

                                // Duplicate images on the right
                                ui.vertical(|ui| {
                                    for path in &paths[1..] {
                                        ui.horizontal(|ui| {
                                            ui.vertical(|ui| {
                                                if let Some(texture) = self.load_texture(ctx, path) {
                                                    let size = texture.size_vec2();
                                                    let aspect = size.x / size.y;
                                                    let height = 200.0;
                                                    ui.image(texture.id(), Vec2::new(height * aspect, height));
                                                }
                                                
                                                let path_clone = path.clone();
                                                let root_clone = root_image.clone();
                                                ui.horizontal(|ui| {
                                                    if ui.button(RichText::new("Keep").size(14.0)).clicked() {
                                                        for other_path in &paths {
                                                            if other_path != &path_clone && other_path != &root_clone {
                                                                self.delete_image(other_path);
                                                            }
                                                        }
                                                    }
                                                    if ui.button(RichText::new("Rename").size(14.0)).clicked() {
                                                        self.rename_image(&path_clone);
                                                    }
                                                    if ui.button(RichText::new("Delete").size(14.0)).clicked() {
                                                        self.delete_image(&path_clone);
                                                    }
                                                });
                                                
                                                ui.add_space(5.0);
                                                // Constrain the width of the path display
                                                ui.with_layout(egui::Layout::left_to_right(egui::Align::LEFT).with_cross_justify(true), |ui| {
                                                    let available_width = ui.available_width().min(300.0);
                                                    ui.set_width(available_width);
                                                    if ui.add(egui::Label::new(self.format_path_with_filename(path))
                                                        .wrap(true)
                                                        .sense(egui::Sense::click()))
                                                        .clicked() {
                                                        self.copy_to_clipboard(path);
                                                        ui.output_mut(|o| o.copied_text = path.to_string());
                                                    }
                                                });
                                            });
                                        });
                                        ui.add_space(10.0);
                                    }
                                });
                            });
                        });
                    }
                });
            }
        });
        
        // Request continuous repaint while scanning
        if self.is_scanning {
            ctx.request_repaint();
        }
    }
}

fn main() {
    let options = eframe::NativeOptions {
        drag_and_drop_support: true,
        initial_window_size: Some([800.0, 600.0].into()),
        min_window_size: Some([800.0, 600.0].into()),
        ..Default::default()
    };
    
    eframe::run_native(
        "Duplicate Image Finder",
        options,
        Box::new(|_cc| Box::new(MyApp::new())),
    );
}
