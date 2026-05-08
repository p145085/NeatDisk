use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use eframe::egui;
use egui::{
    CentralPanel, Color32, Frame, ProgressBar, RichText, ScrollArea, Stroke, TextureOptions, Vec2,
};
use image::imageops::FilterType;
use rfd::FileDialog;

use crate::license;
use crate::scanner::{run_scan, DuplicateGroup, ScanMessage, ScanProgress};
use crate::settings::Settings;

enum AppState {
    Idle,
    Scanning,
    Results,
}

pub struct App {
    state: AppState,
    scan_path: String,
    settings: Settings,
    show_settings: bool,

    rx: Option<Receiver<ScanMessage>>,
    progress: ScanProgress,

    groups: Vec<DuplicateGroup>,
    texture_cache: HashMap<PathBuf, egui::TextureHandle>,
    errors: Vec<String>,

    total_wasted: u64,
    total_groups: usize,
    total_dup_files: usize,
    clearable_count: usize,
    clearable_result: Option<String>,

    is_pro: bool,
    license_key_input: String,
    license_error: Option<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            state: AppState::Idle,
            scan_path: String::new(),
            settings: Settings::load(),
            show_settings: false,
            rx: None,
            progress: ScanProgress { processed: 0, total: 1, phase: "Starting..." },
            groups: Vec::new(),
            texture_cache: HashMap::new(),
            errors: Vec::new(),
            total_wasted: 0,
            total_groups: 0,
            total_dup_files: 0,
            clearable_count: 0,
            clearable_result: None,

            is_pro: license::is_pro(),
            license_key_input: String::new(),
            license_error: None,
        }
    }

    fn start_scan(&mut self) {
        let path = PathBuf::from(&self.scan_path);
        let settings = self.settings.clone();
        self.groups.clear();
        self.texture_cache.clear();
        self.errors.clear();
        self.state = AppState::Scanning;
        self.progress = ScanProgress { processed: 0, total: 1, phase: "Starting..." };
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        thread::spawn(move || run_scan(path, settings, tx));
    }

    fn poll_scan(&mut self) {
        let mut done = None;
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    ScanMessage::Progress(p) => self.progress = p,
                    ScanMessage::Complete(g) => done = Some(g),
                }
            }
        }
        if let Some(groups) = done {
            self.groups = groups;
            self.rx = None;
            self.update_stats();
            // Clearable-folder groups bubble to the top; wasted-bytes order preserved within each partition.
            let cf = self.settings.clearable_folder.as_deref();
            self.groups.sort_by_key(|g| !g.images.iter().any(|img| is_clearable(&img.path, cf)));
            self.state = AppState::Results;
        }
    }

    fn delete_file(&mut self, path: &Path) {
        match trash::delete(path) {
            Ok(()) => {
                self.texture_cache.remove(path);
                for g in &mut self.groups {
                    g.images.retain(|img| img.path != path);
                }
                self.groups.retain(|g| g.images.len() > 1);
                self.update_stats();
            }
            Err(e) => self.errors.push(format!("{}: {}", path.display(), e)),
        }
    }

    fn keep_image(&mut self, keep: &Path) {
        let to_del: Vec<PathBuf> = self
            .groups
            .iter()
            .find(|g| g.images.iter().any(|img| img.path == keep))
            .map(|g| {
                g.images
                    .iter()
                    .filter(|img| img.path != keep)
                    .map(|img| img.path.clone())
                    .collect()
            })
            .unwrap_or_default();
        for p in to_del {
            self.delete_file(&p);
        }
    }

    fn keep_all_best(&mut self) {
        let to_del: Vec<PathBuf> = self
            .groups
            .iter()
            .flat_map(|g| {
                let best = g.best_index();
                g.images
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != best)
                    .map(|(_, img)| img.path.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        for p in to_del {
            self.delete_file(&p);
        }
    }

    fn auto_clean_clearable(&mut self) {
        let cf = match self.settings.clearable_folder.as_deref() {
            Some(cf) => cf.to_owned(),
            None => return,
        };

        let mut deleted: usize = 0;
        let mut moved: usize = 0;

        // Collect work upfront to avoid borrow conflicts during mutation.
        struct GroupWork {
            clearable_paths: Vec<PathBuf>,
            organized_paths: Vec<PathBuf>,
            best_is_clearable: bool,
        }

        let work: Vec<GroupWork> = self.groups.iter().map(|g| {
            let best = g.best_index();
            let best_is_clearable = is_clearable(&g.images[best].path, Some(&cf));
            GroupWork {
                clearable_paths: g.images.iter()
                    .filter(|img| is_clearable(&img.path, Some(&cf)))
                    .map(|img| img.path.clone())
                    .collect(),
                organized_paths: g.images.iter()
                    .filter(|img| !is_clearable(&img.path, Some(&cf)))
                    .map(|img| img.path.clone())
                    .collect(),
                best_is_clearable,
            }
        }).collect();

        for w in &work {
            if w.clearable_paths.is_empty() {
                continue;
            }
            // Edge case: all copies are inside the clearable folder — nothing to move to, skip.
            if w.organized_paths.is_empty() {
                continue;
            }

            if !w.best_is_clearable {
                // Case A: organized copy wins — trash the clearable copies.
                for path in &w.clearable_paths {
                    match trash::delete(path) {
                        Ok(()) => { self.texture_cache.remove(path.as_path()); deleted += 1; }
                        Err(e) => self.errors.push(format!("{}: {}", path.display(), e)),
                    }
                }
            } else {
                // Case B: clearable copy is best — trash organized copies, move clearable into place.
                let dest_dir = w.organized_paths[0].parent().map(|p| p.to_owned());
                for path in &w.organized_paths {
                    match trash::delete(path) {
                        Ok(()) => { self.texture_cache.remove(path.as_path()); deleted += 1; }
                        Err(e) => self.errors.push(format!("{}: {}", path.display(), e)),
                    }
                }
                if let Some(dir) = dest_dir {
                    for src in &w.clearable_paths {
                        match move_file(src, &dir) {
                            Ok(dest) => {
                                self.texture_cache.remove(src.as_path());
                                self.texture_cache.remove(dest.as_path());
                                moved += 1;
                            }
                            Err(e) => self.errors.push(e),
                        }
                    }
                }
            }
        }

        // Prune resolved groups from the list.
        for g in &mut self.groups {
            g.images.retain(|img| img.path.exists());
        }
        self.groups.retain(|g| g.images.len() > 1);
        self.update_stats();

        self.clearable_result = Some(format!("Deleted {}  ·  Moved {} to organized folders", deleted, moved));
    }

    fn update_stats(&mut self) {
        self.total_groups = self.groups.len();
        self.total_dup_files = self.groups.iter().map(|g| g.images.len() - 1).sum();
        self.total_wasted = self.groups.iter().map(|g| g.wasted_bytes()).sum();
        let cf = self.settings.clearable_folder.as_deref();
        self.clearable_count = self.groups.iter()
            .filter(|g| g.images.iter().any(|img| is_clearable(&img.path, cf)))
            .count();
    }

    // Loads from cache or decodes; returns clone of handle.
    fn load_texture(&mut self, ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
        if let Some(h) = self.texture_cache.get(path) {
            return Some(h.clone());
        }
        let img = image::open(path).ok()?;
        let img = if img.width() > 280 || img.height() > 280 {
            img.resize(280, 280, FilterType::Triangle)
        } else {
            img
        };
        let size = [img.width() as usize, img.height() as usize];
        let rgba = img.to_rgba8();
        let ci = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_flat_samples().as_slice());
        let h = ctx.load_texture(path.to_string_lossy(), ci, TextureOptions::default());
        self.texture_cache.insert(path.to_owned(), h.clone());
        Some(h)
    }

    fn fmt_bytes(b: u64) -> String {
        if b < 1024 {
            format!("{} B", b)
        } else if b < 1024 * 1024 {
            format!("{:.1} KB", b as f64 / 1024.0)
        } else {
            format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
        }
    }
}

fn is_clearable(path: &Path, cf: Option<&str>) -> bool {
    cf.map(|root| path.starts_with(root)).unwrap_or(false)
}

fn move_file(from: &Path, to_dir: &Path) -> Result<PathBuf, String> {
    let fname = from.file_name().ok_or_else(|| format!("no filename: {}", from.display()))?;
    let dest = to_dir.join(fname);
    std::fs::rename(from, &dest)
        .or_else(|_| std::fs::copy(from, &dest).and_then(|_| std::fs::remove_file(from)).map(|_| ()))
        .map(|_| dest)
        .map_err(|e| format!("move {} → {}: {}", from.display(), to_dir.display(), e))
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_scan();

        // Pre-load all thumbnails BEFORE entering the render closure.
        // This avoids nested &mut self borrows inside egui closures.
        let groups: Vec<DuplicateGroup> = self.groups.clone();
        let textures: Vec<Vec<Option<egui::TextureHandle>>> = groups
            .iter()
            .map(|g| g.images.iter().map(|img| self.load_texture(ctx, &img.path)).collect())
            .collect();

        let cf = self.settings.clearable_folder.as_deref();
        let clearable_flags: Vec<Vec<bool>> = groups.iter()
            .map(|g| g.images.iter().map(|img| is_clearable(&img.path, cf)).collect())
            .collect();

        // Collect pending mutations; apply after rendering.
        let mut to_delete: Vec<PathBuf> = Vec::new();
        let mut to_keep: Vec<PathBuf> = Vec::new();
        let mut do_keep_all_best = false;
        let mut do_new_scan = false;
        let mut do_auto_clean = false;
        let mut start_scan = false;

        CentralPanel::default().show(ctx, |ui| {
            // ── toolbar ────────────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.label(RichText::new("📁").size(18.0));
                ui.add(
                    egui::TextEdit::singleline(&mut self.scan_path)
                        .hint_text("Select a folder to scan…")
                        .desired_width(ui.available_width() - 180.0),
                );
                if ui.button("Browse").clicked() {
                    if let Some(p) = FileDialog::new().set_title("Select Folder").pick_folder() {
                        self.scan_path = p.to_string_lossy().to_string();
                    }
                }
                let scanning = matches!(self.state, AppState::Scanning);
                ui.add_enabled_ui(!self.scan_path.is_empty() && !scanning, |ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("  Scan  ").color(Color32::WHITE))
                                .fill(Color32::from_rgb(40, 110, 200)),
                        )
                        .clicked()
                    {
                        start_scan = true;
                    }
                });
                if ui.button("⚙").on_hover_text("Settings").clicked() {
                    self.show_settings = !self.show_settings;
                }
                if self.is_pro {
                    ui.label(RichText::new("Pro").small().strong().color(Color32::GOLD));
                }
            });

            // ── settings panel ─────────────────────────────────────────────
            if self.show_settings {
                Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.settings.include_hidden, "Include hidden folders");
                        ui.separator();
                        ui.add_enabled_ui(self.is_pro, |ui| {
                            ui.checkbox(&mut self.settings.use_perceptual, "Similar images");
                        });
                        if !self.is_pro {
                            ui.label(
                                RichText::new("  (Pro)")
                                    .small()
                                    .color(Color32::from_rgb(200, 160, 40)),
                            );
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Clearable folder:");
                        let cf_text = self.settings.clearable_folder
                            .as_deref()
                            .unwrap_or("")
                            .to_owned();
                        let mut cf_buf = cf_text;
                        if ui.add(
                            egui::TextEdit::singleline(&mut cf_buf)
                                .hint_text("Duplicates here are removed first…")
                                .desired_width(ui.available_width() - 80.0),
                        ).changed() {
                            self.settings.clearable_folder = if cf_buf.is_empty() { None } else { Some(cf_buf) };
                        }
                        if ui.button("Browse").clicked() {
                            if let Some(p) = FileDialog::new().set_title("Select Clearable Folder").pick_folder() {
                                self.settings.clearable_folder = Some(p.to_string_lossy().to_string());
                            }
                        }
                        if ui.button("✕").on_hover_text("Clear").clicked() {
                            self.settings.clearable_folder = None;
                        }
                    });
                    ui.label(
                        RichText::new("When duplicates are found: non-best copies in this folder are trashed; if this folder has the best copy, the organized copy is trashed and this file is moved into its place.")
                            .small()
                            .color(Color32::from_rgb(120, 120, 120)),
                    );
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("License key:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.license_key_input)
                                .hint_text("XXXXXXXX-XXXXXXXX-XXXXXXXX-XXXXXXXX")
                                .desired_width(ui.available_width() - 80.0),
                        );
                        if ui.button("Activate").clicked() {
                            let key = self.license_key_input.trim().to_owned();
                            if license::validate_key(&key) {
                                license::save_key(&key);
                                self.is_pro = true;
                                self.license_error = None;
                                self.license_key_input.clear();
                            } else {
                                self.license_error = Some("Invalid license key.".to_owned());
                            }
                        }
                    });
                    if self.is_pro {
                        ui.label(
                            RichText::new("✓ Pro license active")
                                .small()
                                .color(Color32::from_rgb(80, 200, 80)),
                        );
                    } else if let Some(err) = &self.license_error {
                        ui.label(RichText::new(err).small().color(Color32::LIGHT_RED));
                    }

                    if ui.small_button("Save").clicked() {
                        self.settings.save();
                    }
                });
            }

            ui.separator();

            // ── state content ──────────────────────────────────────────────
            let is_scanning = matches!(self.state, AppState::Scanning);
            let is_results = matches!(self.state, AppState::Results);

            if is_scanning {
                ui.add_space(60.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(self.progress.phase).size(15.0));
                    ui.add_space(10.0);
                    let frac = if self.progress.total > 0 {
                        self.progress.processed as f32 / self.progress.total as f32
                    } else {
                        0.0
                    };
                    ui.add(
                        ProgressBar::new(frac)
                            .text(format!("{} / {}", self.progress.processed, self.progress.total))
                            .desired_width(420.0),
                    );
                });
            } else if is_results {
                // ── summary bar ────────────────────────────────────────────
                Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if self.total_groups == 0 {
                            ui.label(
                                RichText::new("✓ No duplicates found.")
                                    .size(15.0)
                                    .color(Color32::from_rgb(80, 200, 80)),
                            );
                        } else {
                            ui.label(
                                RichText::new(format!(
                                    "{} groups  ·  {} duplicate files  ·  {} reclaimable",
                                    self.total_groups,
                                    self.total_dup_files,
                                    Self::fmt_bytes(self.total_wasted)
                                ))
                                .size(14.0),
                            );
                            if self.clearable_count > 0 {
                                ui.label(
                                    RichText::new(format!("  ({} in unsorted)", self.clearable_count))
                                        .size(13.0)
                                        .color(Color32::from_rgb(210, 160, 40)),
                                );
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("New scan").clicked() {
                                do_new_scan = true;
                            }
                            if self.total_groups > 0 {
                                ui.add_space(6.0);
                                if ui
                                    .add(
                                        egui::Button::new("Keep all best")
                                            .fill(Color32::from_rgb(30, 100, 30)),
                                    )
                                    .on_hover_text(
                                        "Keep the highest-resolution image in each group",
                                    )
                                    .clicked()
                                {
                                    do_keep_all_best = true;
                                }
                                if self.clearable_count > 0 {
                                    ui.add_space(6.0);
                                    let cf_name = self.settings.clearable_folder.as_deref()
                                        .and_then(|p| std::path::Path::new(p).file_name())
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_else(|| "unsorted".to_string());
                                    if ui
                                        .add(
                                            egui::Button::new(format!("Auto-clean {}", cf_name))
                                                .fill(Color32::from_rgb(140, 90, 0)),
                                        )
                                        .on_hover_text(
                                            "Delete lower-quality copies from clearable folder.\n\
                                             If the clearable copy is best, move it to the organized folder.",
                                        )
                                        .clicked()
                                    {
                                        do_auto_clean = true;
                                    }
                                }
                            }
                        });
                    });
                    if let Some(msg) = &self.clearable_result {
                        ui.label(RichText::new(msg).small().color(Color32::from_rgb(160, 160, 160)));
                    }
                });

                // ── errors ─────────────────────────────────────────────────
                if !self.errors.is_empty() {
                    Frame::none()
                        .fill(Color32::from_rgb(70, 15, 15))
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            for e in &self.errors {
                                ui.label(RichText::new(e).color(Color32::LIGHT_RED).small());
                            }
                        });
                }

                // ── duplicate groups ───────────────────────────────────────
                if !groups.is_empty() {
                    ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                        for (gi, group) in groups.iter().enumerate() {
                            let best = group.best_index();
                            let wasted = Self::fmt_bytes(group.wasted_bytes());

                            ui.add_space(6.0);
                            Frame::group(ui.style()).show(ui, |ui| {
                                // Group header
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "{} identical files",
                                            group.images.len()
                                        ))
                                        .strong(),
                                    );
                                    ui.label(
                                        RichText::new(format!("— {} wasted", wasted))
                                            .color(Color32::GRAY),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .add(
                                                    egui::Button::new("Keep best")
                                                        .fill(Color32::from_rgb(30, 100, 30)),
                                                )
                                                .on_hover_text(
                                                    "Keep highest resolution, delete rest",
                                                )
                                                .clicked()
                                            {
                                                to_keep.push(group.images[best].path.clone());
                                            }
                                        },
                                    );
                                });

                                ui.separator();

                                // Image cards
                                ui.horizontal_wrapped(|ui| {
                                    for (ii, img) in group.images.iter().enumerate() {
                                        let is_best = ii == best;
                                        let stroke = if is_best {
                                            Stroke::new(2.0, Color32::GOLD)
                                        } else {
                                            Stroke::NONE
                                        };

                                        Frame::none()
                                            .stroke(stroke)
                                            .inner_margin(6.0)
                                            .outer_margin(4.0)
                                            .show(ui, |ui| {
                                                ui.set_max_width(230.0);
                                                ui.vertical(|ui| {
                                                    // thumbnail (pre-loaded above)
                                                    if let Some(Some(tex)) =
                                                        textures.get(gi).and_then(|t| t.get(ii))
                                                    {
                                                        let sz = tex.size_vec2();
                                                        let aspect = sz.x / sz.y;
                                                        let h = 150.0f32;
                                                        ui.image(
                                                            tex.id(),
                                                            Vec2::new(h * aspect, h),
                                                        );
                                                    } else {
                                                        ui.allocate_space(Vec2::new(150.0, 100.0));
                                                        ui.label(
                                                            RichText::new("(unreadable)")
                                                                .small()
                                                                .color(Color32::DARK_GRAY),
                                                        );
                                                    }

                                                    if is_best {
                                                        ui.label(
                                                            RichText::new("★ Best")
                                                                .color(Color32::GOLD)
                                                                .small()
                                                                .strong(),
                                                        );
                                                    }
                                                    if clearable_flags.get(gi).and_then(|f| f.get(ii)).copied().unwrap_or(false) {
                                                        ui.label(
                                                            RichText::new("📤 Unsorted")
                                                                .color(Color32::from_rgb(210, 160, 40))
                                                                .small(),
                                                        );
                                                    }

                                                    // metadata
                                                    ui.label(
                                                        RichText::new(format!(
                                                            "{}×{}  {}",
                                                            img.width,
                                                            img.height,
                                                            Self::fmt_bytes(img.file_size)
                                                        ))
                                                        .small()
                                                        .color(Color32::GRAY),
                                                    );

                                                    let fname = img
                                                        .path
                                                        .file_name()
                                                        .map(|f| f.to_string_lossy().to_string())
                                                        .unwrap_or_default();
                                                    let folder = img
                                                        .path
                                                        .parent()
                                                        .map(|p| p.to_string_lossy().to_string())
                                                        .unwrap_or_default();
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(&folder)
                                                                .monospace()
                                                                .small()
                                                                .color(Color32::GRAY),
                                                        )
                                                        .wrap(true),
                                                    );
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(&fname)
                                                                .monospace()
                                                                .small()
                                                                .color(Color32::from_rgb(
                                                                    200, 140, 60,
                                                                )),
                                                        )
                                                        .wrap(true),
                                                    )
                                                    .on_hover_text(img.path.to_string_lossy());

                                                    // action buttons
                                                    ui.horizontal(|ui| {
                                                        if ui
                                                            .add(
                                                                egui::Button::new("Keep")
                                                                    .fill(Color32::from_rgb(
                                                                        30, 90, 30,
                                                                    )),
                                                            )
                                                            .on_hover_text(
                                                                "Delete all others in group",
                                                            )
                                                            .clicked()
                                                        {
                                                            to_keep.push(img.path.clone());
                                                        }
                                                        if ui
                                                            .add(
                                                                egui::Button::new("Delete")
                                                                    .fill(Color32::from_rgb(
                                                                        120, 30, 30,
                                                                    )),
                                                            )
                                                            .on_hover_text("Move to Recycle Bin")
                                                            .clicked()
                                                        {
                                                            to_delete.push(img.path.clone());
                                                        }
                                                    });
                                                });
                                            });
                                    }
                                });
                            });
                        }
                    });
                }
            } else {
                // Idle
                ui.add_space(80.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("Select a folder and click Scan")
                            .size(20.0)
                            .color(Color32::GRAY),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            "Exact duplicates are found by checksum (fast).\n\
                             Similar/resized images can be found with dHash (Pro).",
                        )
                        .size(12.0)
                        .color(Color32::from_rgb(90, 90, 90)),
                    );
                });
            }
        });

        // ── apply mutations after render ───────────────────────────────────
        if start_scan {
            self.start_scan();
        }
        if do_new_scan {
            self.state = AppState::Idle;
            self.groups.clear();
            self.texture_cache.clear();
            self.errors.clear();
            self.clearable_result = None;
        }
        if do_keep_all_best {
            self.keep_all_best();
        }
        if do_auto_clean {
            self.auto_clean_clearable();
        }
        for p in to_keep {
            self.keep_image(&p);
        }
        for p in to_delete {
            self.delete_file(&p);
        }

        if matches!(self.state, AppState::Scanning) {
            ctx.request_repaint();
        }
    }
}
