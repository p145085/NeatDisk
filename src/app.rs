use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui;
use egui::{
    CentralPanel, Color32, Frame, ProgressBar, RichText, ScrollArea, Stroke, TextureOptions, Vec2,
};
use image::imageops::FilterType;
use rfd::FileDialog;

use crate::tray::AppTray;
use crate::cleaner::{self, CleanerMsg, JunkCategory};
use crate::disk_analyzer::{self, DiskAnalysis, DiskMsg};
use crate::large_files::{scan_large_files, LargeFileEntry, LargeFileMsg};
use crate::license;
use crate::scanner::{log_path, run_scan, DuplicateGroup, ScanMessage, ScanProgress, ScanResult};
use crate::settings::Settings;
use crate::updater::{self, UpdateInfo};
use crate::scheduler;

const PRO_PURCHASE_URL: &str = "https://emiljohansson.info/softwares/neat-disk";
const FREE_LARGE_FILE_LIMIT: usize = 20;

#[derive(PartialEq)]
enum AppTab {
    Duplicates,
    LargeFiles,
    Cleaner,
    DiskAnalyzer,
}

#[derive(PartialEq)]
enum CleanerState {
    Idle,
    Analyzing,
    Ready,
    Cleaning,
    Done,
}

#[derive(PartialEq)]
enum DiskAnalyzerState {
    Idle,
    Scanning,
    Results,
}

enum AppState {
    Idle,
    Scanning,
    Summary,
    Results,
    Comparing(usize), // group index
}

enum LargeFileState {
    Idle,
    Scanning,
    Results,
}

pub struct App {
    // Shared
    active_tab: AppTab,
    scan_path: String,
    settings: Settings,
    show_settings: bool,
    show_about: bool,
    is_pro: bool,
    license_key_input: String,
    license_error: Option<String>,
    show_pro_modal: bool,
    pro_modal_feature: &'static str,
    show_confirm_clean: bool,

    // Duplicates tab
    state: AppState,
    cancel_flag: Arc<AtomicBool>,
    scan_pause_flag: Arc<AtomicBool>,
    rx: Option<Receiver<ScanMessage>>,
    progress: ScanProgress,
    groups: Vec<DuplicateGroup>,
    texture_cache: HashMap<PathBuf, egui::TextureHandle>,
    text_preview_cache: HashMap<PathBuf, String>,
    errors: Vec<String>,
    total_wasted: u64,
    image_wasted: u64,
    video_wasted: u64,
    total_groups: usize,
    total_dup_files: usize,
    clearable_count: usize,
    clearable_result: Option<String>,
    scan_file_count: usize,
    scan_duration_secs: f64,
    total_scanned_bytes: u64,
    scan_id: u64,

    // Background texture loader
    tex_rx: Option<Receiver<(PathBuf, egui::ColorImage)>>,

    // Large files tab
    lf_state: LargeFileState,
    lf_rx: Option<Receiver<LargeFileMsg>>,
    lf_files: Vec<LargeFileEntry>,
    lf_threshold_mb: u64,
    lf_cancel_flag: Arc<AtomicBool>,
    lf_pause_flag: Arc<AtomicBool>,

    // Cleaner tab
    cl_state: CleanerState,
    cl_rx: Option<Receiver<CleanerMsg>>,
    cl_cancel_flag: Arc<AtomicBool>,
    cl_pause_flag: Arc<AtomicBool>,
    cl_categories: Vec<JunkCategory>,
    cl_progress: (usize, usize),
    cl_freed_bytes: u64,

    // Disk Analyzer tab
    da_state: DiskAnalyzerState,
    da_rx: Option<Receiver<DiskMsg>>,
    da_result: Option<DiskAnalysis>,
    da_progress: usize,
    da_cancel_flag: Arc<AtomicBool>,
    da_pause_flag: Arc<AtomicBool>,

    // Empty folders (from duplicate scan)
    empty_folders: Vec<PathBuf>,
    show_empty_folders: bool,

    // Auto-update
    update_rx: Option<Receiver<Option<UpdateInfo>>>,
    update_info: Option<UpdateInfo>,

    // Scheduled scan
    schedule_error: Option<String>,
    auto_scan_pending: bool,

    // System tray
    tray: Option<AppTray>,
    hwnd: Option<isize>,    // cached HWND for show/hide via winapi
    window_visible: bool,
    quit_requested: bool,   // true when Quit is chosen from tray (bypasses hide-on-close)
}

impl App {
    pub fn new(auto_scan: bool) -> Self {
        let mut settings = Settings::load();
        // Reconcile saved preference with the actual Task Scheduler state so the
        // checkbox is correct even if the user manually deleted the task.
        settings.schedule_enabled = scheduler::is_registered();
        let lf_threshold = settings.large_file_threshold_mb;
        let minimize_to_tray = settings.minimize_to_tray;
        // When launched by scheduler use the most-recent folder, else default to C:\.
        let scan_path = if auto_scan {
            settings.recent_folders.first().cloned().unwrap_or_else(|| "C:\\".to_string())
        } else {
            settings.recent_folders.first().cloned().unwrap_or_else(|| "C:\\".to_string())
        };
        Self {
            active_tab: AppTab::Duplicates,
            scan_path,
            settings,
            show_settings: false,
            show_about: false,
            is_pro: license::is_pro(),
            license_key_input: String::new(),
            license_error: None,
            show_pro_modal: false,
            pro_modal_feature: "",
            show_confirm_clean: false,

            state: AppState::Idle,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            scan_pause_flag: Arc::new(AtomicBool::new(false)),
            rx: None,
            progress: ScanProgress { processed: 0, total: 1, phase: "Starting..." },
            groups: Vec::new(),
            texture_cache: HashMap::new(),
            text_preview_cache: HashMap::new(),
            errors: Vec::new(),
            total_wasted: 0,
            image_wasted: 0,
            video_wasted: 0,
            total_groups: 0,
            total_dup_files: 0,
            clearable_count: 0,
            clearable_result: None,
            scan_file_count: 0,
            scan_duration_secs: 0.0,
            total_scanned_bytes: 0,
            scan_id: 0,

            tex_rx: None,

            lf_state: LargeFileState::Idle,
            lf_rx: None,
            lf_files: Vec::new(),
            lf_threshold_mb: lf_threshold,
            lf_cancel_flag: Arc::new(AtomicBool::new(false)),
            lf_pause_flag: Arc::new(AtomicBool::new(false)),

            cl_state: CleanerState::Idle,
            cl_rx: None,
            cl_categories: cleaner::resolve_categories(),
            cl_progress: (0, 0),
            cl_freed_bytes: 0,
            cl_cancel_flag: Arc::new(AtomicBool::new(false)),
            cl_pause_flag: Arc::new(AtomicBool::new(false)),

            da_state: DiskAnalyzerState::Idle,
            da_rx: None,
            da_result: None,
            da_progress: 0,
            da_cancel_flag: Arc::new(AtomicBool::new(false)),
            da_pause_flag: Arc::new(AtomicBool::new(false)),

            empty_folders: Vec::new(),
            show_empty_folders: false,

            update_rx: {
                let (tx, rx) = mpsc::channel();
                updater::spawn_check(tx);
                Some(rx)
            },
            update_info: None,

            schedule_error: None,
            auto_scan_pending: auto_scan,

            tray: if minimize_to_tray { AppTray::build() } else { None },
            hwnd: None,
            window_visible: true,
            quit_requested: false,
        }
    }

    fn hide_window(&mut self) {
        if let Some(raw) = self.hwnd {
            unsafe {
                use winapi::um::winuser::{ShowWindow, SW_HIDE};
                ShowWindow(raw as _, SW_HIDE);
            }
            self.window_visible = false;
        }
    }

    fn show_window_to_front(&mut self) {
        if let Some(raw) = self.hwnd {
            unsafe {
                use winapi::um::winuser::{SetForegroundWindow, ShowWindow, SW_RESTORE};
                ShowWindow(raw as _, SW_RESTORE);
                SetForegroundWindow(raw as _);
            }
            self.window_visible = true;
        }
    }

    fn poll_tray(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        use tray_icon::{menu::MenuEvent, TrayIconEvent};

        // Any click/interaction on the tray icon when window is hidden → show it.
        while TrayIconEvent::receiver().try_recv().is_ok() {
            if !self.window_visible {
                self.show_window_to_front();
                ctx.request_repaint();
            }
        }

        // Extract item IDs before mutably borrowing self for actions.
        let (show_id, scan_id, quit_id) = match &self.tray {
            Some(t) => (t.show_item_id.clone(), t.scan_item_id.clone(), t.quit_item_id.clone()),
            None => return,
        };

        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == show_id {
                if self.window_visible {
                    self.hide_window();
                } else {
                    self.show_window_to_front();
                }
                ctx.request_repaint();
            } else if ev.id == scan_id {
                self.show_window_to_front();
                if !self.scan_path.is_empty() && !matches!(self.state, AppState::Scanning) {
                    self.active_tab = AppTab::Duplicates;
                    self.start_scan();
                }
                ctx.request_repaint();
            } else if ev.id == quit_id {
                self.quit_requested = true;
                frame.close();
            }
        }
    }

    fn start_scan(&mut self) {
        let path = PathBuf::from(&self.scan_path);
        self.settings.push_recent(&self.scan_path.clone());
        self.settings.save();
        let settings = self.settings.clone();
        self.groups.clear();
        self.texture_cache.clear();
        self.text_preview_cache.clear();
        self.tex_rx = None;
        self.errors.clear();
        self.cancel_flag.store(false, Ordering::Relaxed);
        self.scan_pause_flag.store(false, Ordering::Relaxed);
        let cancel = self.cancel_flag.clone();
        let pause = self.scan_pause_flag.clone();
        self.state = AppState::Scanning;
        self.progress = ScanProgress { processed: 0, total: 1, phase: "Starting..." };
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        thread::spawn(move || run_scan(path, settings, tx, cancel, pause));
    }

    fn start_large_file_scan(&mut self) {
        let path = PathBuf::from(&self.scan_path);
        let min_bytes = self.lf_threshold_mb * 1024 * 1024;
        self.lf_files.clear();
        self.lf_state = LargeFileState::Scanning;
        self.lf_cancel_flag.store(false, Ordering::Relaxed);
        self.lf_pause_flag.store(false, Ordering::Relaxed);
        let cancel = self.lf_cancel_flag.clone();
        let pause = self.lf_pause_flag.clone();
        let (tx, rx) = mpsc::channel();
        self.lf_rx = Some(rx);
        thread::spawn(move || scan_large_files(path, min_bytes, tx, cancel, pause));
    }

    fn poll_scan(&mut self) {
        let mut done: Option<ScanResult> = None;
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    ScanMessage::Progress(p) => self.progress = p,
                    ScanMessage::Complete(result) => done = Some(result),
                    ScanMessage::Cancelled => {
                        self.rx = None;
                        self.state = AppState::Idle;
                        return;
                    }
                }
            }
        }
        if let Some(result) = done {
            self.scan_file_count = result.file_count;
            self.scan_duration_secs = result.duration_secs;
            self.total_scanned_bytes = result.total_scanned_bytes;
            self.groups = result.groups;
            self.empty_folders = result.empty_folders;
            self.show_empty_folders = false;
            self.rx = None;
            self.update_stats();
            let cf = self.settings.clearable_folder.as_deref();
            self.groups.sort_by_key(|g| !g.images.iter().any(|img| is_clearable(&img.path, cf)));
            self.scan_id += 1;
            self.state = AppState::Summary;
            // If the scan was triggered by the scheduler and found duplicates, pop the
            // window back up so the user notices.
            if !self.window_visible && !self.groups.is_empty() {
                self.show_window_to_front();
            }
        }
    }

    fn poll_large_files(&mut self) {
        use std::sync::mpsc::TryRecvError;
        if let Some(rx) = &self.lf_rx {
            loop {
                match rx.try_recv() {
                    Ok(LargeFileMsg::Done(files)) => {
                        self.lf_files = files;
                        self.lf_rx = None;
                        self.lf_state = LargeFileState::Results;
                        break;
                    }
                    Ok(LargeFileMsg::Cancelled) => {
                        self.lf_rx = None;
                        self.lf_state = LargeFileState::Idle;
                        break;
                    }
                    Err(TryRecvError::Disconnected) => { self.lf_rx = None; break; }
                    Err(TryRecvError::Empty) => break,
                }
            }
        }
    }

    fn poll_cleaner(&mut self) {
        use std::sync::mpsc::TryRecvError;
        if let Some(rx) = &self.cl_rx {
            loop {
                match rx.try_recv() {
                    Ok(CleanerMsg::Progress(n, t)) => { self.cl_progress = (n, t); }
                    Ok(CleanerMsg::AnalyzeDone(cats)) => {
                        self.cl_categories = cats;
                        self.cl_state = CleanerState::Ready;
                        self.cl_rx = None;
                        break;
                    }
                    Ok(CleanerMsg::CleanDone { freed_bytes }) => {
                        self.cl_freed_bytes = freed_bytes;
                        self.cl_state = CleanerState::Done;
                        self.cl_rx = None;
                        break;
                    }
                    Ok(CleanerMsg::Cancelled) => {
                        self.cl_state = CleanerState::Idle;
                        self.cl_rx = None;
                        break;
                    }
                    Err(TryRecvError::Disconnected) => { self.cl_rx = None; break; }
                    _ => break,
                }
            }
        }
    }

    fn poll_disk_analyzer(&mut self) {
        use std::sync::mpsc::TryRecvError;
        if let Some(rx) = &self.da_rx {
            loop {
                match rx.try_recv() {
                    Ok(DiskMsg::Progress(n)) => { self.da_progress = n; }
                    Ok(DiskMsg::Done(result)) => {
                        self.da_result = Some(result);
                        self.da_state = DiskAnalyzerState::Results;
                        self.da_rx = None;
                        break;
                    }
                    Ok(DiskMsg::Cancelled) => {
                        self.da_state = DiskAnalyzerState::Idle;
                        self.da_rx = None;
                        break;
                    }
                    Err(TryRecvError::Disconnected) => { self.da_rx = None; break; }
                    _ => break,
                }
            }
        }
    }

    fn poll_update_check(&mut self) {
        if let Some(rx) = &self.update_rx {
            if let Ok(info) = rx.try_recv() {
                self.update_info = info;
                self.update_rx = None;
            }
        }
    }

    fn start_cleaner_analyze(&mut self) {
        let cats = cleaner::resolve_categories();
        self.cl_state = CleanerState::Analyzing;
        self.cl_progress = (0, cats.len());
        self.cl_cancel_flag.store(false, Ordering::Relaxed);
        self.cl_pause_flag.store(false, Ordering::Relaxed);
        let cancel = self.cl_cancel_flag.clone();
        let pause = self.cl_pause_flag.clone();
        let (tx, rx) = mpsc::channel();
        self.cl_rx = Some(rx);
        thread::spawn(move || cleaner::analyze(cats, tx, cancel, pause));
    }

    fn start_cleaner_clean(&mut self) {
        let cats = self.cl_categories.clone();
        self.cl_state = CleanerState::Cleaning;
        let (tx, rx) = mpsc::channel();
        self.cl_rx = Some(rx);
        thread::spawn(move || cleaner::clean(&cats, tx));
    }

    fn start_disk_analyze(&mut self) {
        let root = PathBuf::from(&self.scan_path);
        self.da_state = DiskAnalyzerState::Scanning;
        self.da_progress = 0;
        self.da_cancel_flag.store(false, Ordering::Relaxed);
        self.da_pause_flag.store(false, Ordering::Relaxed);
        let cancel = self.da_cancel_flag.clone();
        let pause  = self.da_pause_flag.clone();
        let (tx, rx) = mpsc::channel();
        self.da_rx = Some(rx);
        thread::spawn(move || disk_analyzer::analyze(root, tx, cancel, pause));
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

        struct GroupWork {
            clearable_paths: Vec<PathBuf>,
            organized_paths: Vec<PathBuf>,
            best_is_clearable: bool,
        }

        let work: Vec<GroupWork> = self
            .groups
            .iter()
            .map(|g| {
                let best = g.best_index();
                let best_is_clearable = is_clearable(&g.images[best].path, Some(&cf));
                GroupWork {
                    clearable_paths: g
                        .images
                        .iter()
                        .filter(|img| is_clearable(&img.path, Some(&cf)))
                        .map(|img| img.path.clone())
                        .collect(),
                    organized_paths: g
                        .images
                        .iter()
                        .filter(|img| !is_clearable(&img.path, Some(&cf)))
                        .map(|img| img.path.clone())
                        .collect(),
                    best_is_clearable,
                }
            })
            .collect();

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
                        Ok(()) => {
                            self.texture_cache.remove(path.as_path());
                            deleted += 1;
                        }
                        Err(e) => self.errors.push(format!("{}: {}", path.display(), e)),
                    }
                }
            } else {
                // Case B: clearable copy is best — trash organized copies, move clearable into place.
                let dest_dir = w.organized_paths[0].parent().map(|p| p.to_owned());
                for path in &w.organized_paths {
                    match trash::delete(path) {
                        Ok(()) => {
                            self.texture_cache.remove(path.as_path());
                            deleted += 1;
                        }
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

        for g in &mut self.groups {
            g.images.retain(|img| img.path.exists());
        }
        self.groups.retain(|g| g.images.len() > 1);
        self.update_stats();

        self.clearable_result =
            Some(format!("Deleted {}  ·  Moved {} to organized folders", deleted, moved));
    }

    fn update_stats(&mut self) {
        self.total_groups = self.groups.len();
        self.total_dup_files = self.groups.iter().map(|g| g.images.len() - 1).sum();
        self.total_wasted = self.groups.iter().map(|g| g.wasted_bytes()).sum();
        self.video_wasted = self
            .groups
            .iter()
            .filter(|g| g.images.first().map(|img| is_video_path(&img.path)).unwrap_or(false))
            .map(|g| g.wasted_bytes())
            .sum();
        self.image_wasted = self.total_wasted.saturating_sub(self.video_wasted);
        let cf = self.settings.clearable_folder.as_deref();
        self.clearable_count = self
            .groups
            .iter()
            .filter(|g| g.images.iter().any(|img| is_clearable(&img.path, cf)))
            .count();
    }

    fn fmt_bytes(b: u64) -> String {
        if b < 1024 {
            format!("{} B", b)
        } else if b < 1024 * 1024 {
            format!("{:.1} KB", b as f64 / 1024.0)
        } else if b < 1024 * 1024 * 1024 {
            format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.2} GB", b as f64 / (1024.0 * 1024.0 * 1024.0))
        }
    }
}

fn is_clearable(path: &Path, cf: Option<&str>) -> bool {
    cf.map(|root| path.starts_with(root)).unwrap_or(false)
}

fn is_video_path(path: &Path) -> bool {
    const VIDEO_EXTS: &[&str] = &["mp4", "mov", "avi", "mkv", "webm", "flv", "wmv", "m4v"];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.iter().any(|v| v.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

fn move_file(from: &Path, to_dir: &Path) -> Result<PathBuf, String> {
    let fname = from.file_name().ok_or_else(|| format!("no filename: {}", from.display()))?;
    let dest = to_dir.join(fname);
    std::fs::rename(from, &dest)
        .or_else(|_| {
            std::fs::copy(from, &dest).and_then(|_| std::fs::remove_file(from)).map(|_| ())
        })
        .map(|_| dest)
        .map_err(|e| format!("move {} → {}: {}", from.display(), to_dir.display(), e))
}

fn file_type_icon(path: &Path) -> (&'static str, Color32) {
    const VIDEO_EXTS: &[&str] = &["mp4", "mov", "avi", "mkv", "webm", "flv", "wmv", "m4v"];
    const AUDIO_EXTS: &[&str] = &["mp3", "flac", "wav", "aac", "ogg", "m4a", "wma", "opus", "aiff"];
    const DOC_EXTS: &[&str] = &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "rtf", "odt", "ods", "odp", "csv", "md"];
    const ARCHIVE_EXTS: &[&str] = &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "cab"];
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let ext = ext.as_str();
    if VIDEO_EXTS.contains(&ext) {
        ("Video", Color32::from_rgb(50, 70, 160))
    } else if AUDIO_EXTS.contains(&ext) {
        ("Audio", Color32::from_rgb(100, 50, 150))
    } else if DOC_EXTS.contains(&ext) {
        ("Document", Color32::from_rgb(40, 90, 140))
    } else if ARCHIVE_EXTS.contains(&ext) {
        ("Archive", Color32::from_rgb(90, 60, 20))
    } else {
        ("File", Color32::from_rgb(55, 55, 55))
    }
}

fn is_text_preview_type(path: &Path) -> bool {
    const EXTS: &[&str] = &["txt", "csv", "md", "log", "json", "xml", "html", "htm", "toml", "yaml", "yml", "ini", "cfg"];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| EXTS.iter().any(|t| t.eq_ignore_ascii_case(ext)))
        .unwrap_or(false)
}

fn load_text_preview(path: &Path) -> String {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    let cap = bytes.len().min(800);
    let s = String::from_utf8_lossy(&bytes[..cap]).to_string();
    s.lines()
        .take(6)
        .map(|l| if l.len() > 30 { format!("{}…", &l[..30]) } else { l.to_string() })
        .collect::<Vec<_>>()
        .join("\n")
}

fn open_file(path: &Path) {
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", &path.to_string_lossy()])
        .spawn();
}

fn decode_thumbnail(path: &Path) -> Option<egui::ColorImage> {
    if is_video_path(path) {
        return decode_video_thumbnail(path);
    }
    let img = image::open(path).ok()?;
    let img = if img.width() > 280 || img.height() > 280 {
        img.resize(280, 280, FilterType::Triangle)
    } else {
        img
    };
    let size = [img.width() as usize, img.height() as usize];
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_flat_samples().as_slice()))
}

fn decode_video_thumbnail(path: &Path) -> Option<egui::ColorImage> {
    use winapi::shared::guiddef::GUID;
    use winapi::shared::winerror::S_OK;
    use winapi::shared::windef::{HBITMAP, SIZE};
    use winapi::um::combaseapi::CoInitializeEx;
    use winapi::um::objbase::COINIT_MULTITHREADED;
    use winapi::um::shobjidl_core::IShellItem;
    use winapi::um::wingdi::{
        DeleteObject, GetDIBits, GetObjectA, BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
        DIB_RGB_COLORS,
    };
    use winapi::um::winuser::{GetDC, ReleaseDC};
    use winapi::Interface;

    // IShellItemImageFactory is not exposed by winapi-rs 0.3 — define it manually.
    // IID: {BCC18B79-BA16-442F-80C4-8A59C30C463B}
    const FACTORY_IID: GUID = GUID {
        Data1: 0xBCC18B79,
        Data2: 0xBA16,
        Data3: 0x442F,
        Data4: [0x80, 0xC4, 0x8A, 0x59, 0xC3, 0x0C, 0x46, 0x3B],
    };

    #[repr(C)]
    struct IShellItemImageFactoryVtbl {
        query_interface: unsafe extern "system" fn(*mut IShellItemImageFactory, *const GUID, *mut *mut std::ffi::c_void) -> i32,
        add_ref: unsafe extern "system" fn(*mut IShellItemImageFactory) -> u32,
        release: unsafe extern "system" fn(*mut IShellItemImageFactory) -> u32,
        get_image: unsafe extern "system" fn(*mut IShellItemImageFactory, SIZE, u32, *mut HBITMAP) -> i32,
    }

    #[repr(C)]
    struct IShellItemImageFactory {
        vtbl: *const IShellItemImageFactoryVtbl,
    }

    unsafe {
        CoInitializeEx(std::ptr::null_mut(), COINIT_MULTITHREADED);

        let wide: Vec<u16> = path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let mut item: *mut IShellItem = std::ptr::null_mut();
        if winapi::um::shobjidl_core::SHCreateItemFromParsingName(
            wide.as_ptr(),
            std::ptr::null_mut(),
            &IShellItem::uuidof(),
            &mut item as *mut _ as _,
        ) != S_OK || item.is_null() {
            return None;
        }

        let mut factory: *mut IShellItemImageFactory = std::ptr::null_mut();
        let hr = (*item).QueryInterface(
            &FACTORY_IID,
            &mut factory as *mut _ as _,
        );
        (*item).Release();
        if hr != S_OK || factory.is_null() {
            return None;
        }

        let sz = SIZE { cx: 280, cy: 280 };
        let mut hbitmap: HBITMAP = std::ptr::null_mut();
        // SIIGBF_RESIZETOFIT = 0: thumbnail fits within the requested size.
        let hr = ((*(*factory).vtbl).get_image)(factory, sz, 0, &mut hbitmap);
        ((*(*factory).vtbl).release)(factory);
        if hr != S_OK || hbitmap.is_null() {
            return None;
        }

        // Query actual bitmap dimensions — GetImage may return a different size.
        let mut bmp: BITMAP = std::mem::zeroed();
        if GetObjectA(
            hbitmap as *mut _,
            std::mem::size_of::<BITMAP>() as i32,
            &mut bmp as *mut _ as *mut _,
        ) == 0 {
            DeleteObject(hbitmap as *mut _);
            return None;
        }
        let w = bmp.bmWidth as u32;
        let h = bmp.bmHeight.unsigned_abs();
        if w == 0 || h == 0 {
            DeleteObject(hbitmap as *mut _);
            return None;
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // negative = top-down row order
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [std::mem::zeroed()],
        };

        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(std::ptr::null_mut());
        let rows = GetDIBits(
            dc,
            hbitmap,
            0,
            h,
            pixels.as_mut_ptr() as *mut _,
            &bmi as *const _ as *mut _,
            DIB_RGB_COLORS,
        );
        ReleaseDC(std::ptr::null_mut(), dc);
        DeleteObject(hbitmap as *mut _);

        if rows == 0 {
            return None;
        }

        // GDI returns BGRA — swap B and R channels, force full opacity.
        for p in pixels.chunks_exact_mut(4) {
            p.swap(0, 2);
            p[3] = 255;
        }

        Some(egui::ColorImage::from_rgba_unmultiplied(
            [w as usize, h as usize],
            &pixels,
        ))
    }
}

fn open_in_explorer(path: &Path) {
    // Run on a background thread: SHParseDisplayName + SHOpenFolderAndSelectItems can
    // block for several seconds (network paths, cold shell cache) and must not run on
    // the egui update thread.
    let path = path.to_path_buf();
    std::thread::spawn(move || open_in_explorer_bg(path));
}

fn open_in_explorer_bg(path: PathBuf) {
    use winapi::ctypes::c_void;
    use winapi::shared::winerror::S_OK;
    use winapi::um::combaseapi::{CoInitializeEx, CoTaskMemFree};
    use winapi::um::libloaderapi::{GetProcAddress, LoadLibraryA};
    use winapi::um::objbase::COINIT_APARTMENTTHREADED;

    type ParseFn = unsafe extern "system" fn(
        *const u16, *mut c_void, *mut *mut c_void, u32, *mut u32,
    ) -> i32;
    type OpenSelectFn = unsafe extern "system" fn(
        *const c_void, u32, *const *const c_void, u32,
    ) -> i32;

    let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.clone());
    let parent_wide: Vec<u16> = parent.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    let file_wide: Vec<u16>   = path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);

        let shell32    = LoadLibraryA(b"shell32.dll\0".as_ptr() as _);
        if shell32.is_null() { return; }
        let parse_ptr  = GetProcAddress(shell32, b"SHParseDisplayName\0".as_ptr() as _);
        let open_ptr   = GetProcAddress(shell32, b"SHOpenFolderAndSelectItems\0".as_ptr() as _);
        if parse_ptr.is_null() || open_ptr.is_null() { return; }

        let sh_parse: ParseFn      = std::mem::transmute(parse_ptr);
        let sh_open: OpenSelectFn  = std::mem::transmute(open_ptr);
        let mut sfgao: u32 = 0;

        // PIDL for the parent folder (needed as pidlFolder argument).
        let mut folder_pidl: *mut c_void = std::ptr::null_mut();
        if sh_parse(parent_wide.as_ptr(), std::ptr::null_mut(), &mut folder_pidl, 0, &mut sfgao) != S_OK
            || folder_pidl.is_null()
        {
            return;
        }

        // Absolute PIDL for the file.  The child PIDL (relative to folder) starts
        // immediately after the folder's items in the byte stream.
        let mut file_pidl: *mut c_void = std::ptr::null_mut();
        if sh_parse(file_wide.as_ptr(), std::ptr::null_mut(), &mut file_pidl, 0, &mut sfgao) == S_OK
            && !file_pidl.is_null()
        {
            // Walk the folder PIDL to find how many bytes its items occupy.
            let folder_data_len = {
                let p = folder_pidl as *const u8;
                let mut off = 0usize;
                loop {
                    let cb = u16::from_le_bytes([*p.add(off), *p.add(off + 1)]);
                    if cb == 0 { break; }
                    off += cb as usize;
                }
                off
            };
            // child_ptr points at the last item of file_pidl (the file's own ID).
            let child_ptr = (file_pidl as *const u8).add(folder_data_len) as *const c_void;
            sh_open(folder_pidl, 1, &child_ptr as *const _ as _, 0);
            CoTaskMemFree(file_pidl);
        }
        CoTaskMemFree(folder_pidl);
    }
}

fn open_url(url: &str) {
    let _ = std::process::Command::new("cmd").args(["/c", "start", "", url]).spawn();
}

fn export_csv(groups: &[DuplicateGroup]) {
    use std::io::Write;
    let Some(path) = rfd::FileDialog::new()
        .set_title("Export duplicates to CSV")
        .set_file_name("duplicates.csv")
        .add_filter("CSV", &["csv"])
        .save_file()
    else {
        return;
    };
    let Ok(mut f) = std::fs::File::create(&path) else { return };
    let _ = writeln!(f, "group_id,is_best,file_path,size_bytes,width,height");
    for (gi, group) in groups.iter().enumerate() {
        let best = group.best_index();
        for (ii, img) in group.images.iter().enumerate() {
            let quoted = format!("\"{}\"", img.path.to_string_lossy().replace('"', "\"\""));
            let _ = writeln!(
                f,
                "{},{},{},{},{},{}",
                gi + 1,
                ii == best,
                quoted,
                img.file_size,
                img.width,
                img.height,
            );
        }
    }
    // Open the file after writing.
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", &path.to_string_lossy()])
        .spawn();
}

/// Finds the app's main window HWND by title using FindWindowW.
/// Called once on the first rendered frame; returns None if not yet ready.
fn find_app_hwnd() -> Option<isize> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::winuser::FindWindowW;

    let title: Vec<u16> = OsStr::new("Disk Cleaner & Duplicate Finder")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() { None } else { Some(hwnd as isize) }
}

impl eframe::App for App {
    fn on_close_event(&mut self) -> bool {
        if self.tray.is_some() && self.settings.minimize_to_tray && !self.quit_requested {
            self.hide_window();
            false // intercept — hide to tray instead of closing
        } else {
            true // allow close (no tray, or Quit chosen from tray menu)
        }
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Capture the native HWND on the first frame so hide/show work.
        if self.hwnd.is_none() {
            self.hwnd = find_app_hwnd();
        }

        // When launched by the scheduler: hide window immediately and start scan.
        // The window will re-appear once duplicates are found (handled in poll_scan).
        if self.auto_scan_pending && self.hwnd.is_some() {
            self.auto_scan_pending = false;
            if !self.scan_path.is_empty() {
                if self.tray.is_some() {
                    self.hide_window();
                }
                self.start_scan();
            }
        }

        self.poll_tray(ctx, frame);

        self.poll_scan();
        self.poll_large_files();
        self.poll_cleaner();
        self.poll_disk_analyzer();
        self.poll_update_check();

        // Drag & drop: accept a folder dropped onto the window.
        let (dropped, is_drag_hovering) = ctx.input(|i| {
            (i.raw.dropped_files.clone(), !i.raw.hovered_files.is_empty())
        });
        for f in &dropped {
            if let Some(p) = &f.path {
                if p.is_dir() {
                    self.scan_path = p.to_string_lossy().to_string();
                    break;
                }
            }
        }

        let groups: Vec<DuplicateGroup> = self.groups.clone();

        // Poll background texture loader — upload decoded frames on the UI thread
        // (ctx.load_texture must run here), drain up to 20 per frame.
        {
            let mut received = 0;
            loop {
                match self.tex_rx.as_ref().map(|rx| rx.try_recv()) {
                    Some(Ok((path, ci))) => {
                        if !self.texture_cache.contains_key(&path) {
                            let h = ctx.load_texture(
                                path.to_string_lossy(),
                                ci,
                                TextureOptions::default(),
                            );
                            self.texture_cache.insert(path, h);
                        }
                        received += 1;
                        if received >= 20 {
                            break;
                        }
                    }
                    Some(Err(TryRecvError::Disconnected)) => {
                        self.tex_rx = None;
                        break;
                    }
                    _ => break,
                }
            }
            if received > 0 {
                ctx.request_repaint();
            }
        }

        // Kick off background loader when entering Results/Comparing and loader isn't running.
        if matches!(self.state, AppState::Results | AppState::Comparing(_)) && self.tex_rx.is_none() {
            let uncached: Vec<PathBuf> = groups
                .iter()
                .flat_map(|g| g.images.iter().map(|img| img.path.clone()))
                .filter(|p| !self.texture_cache.contains_key(p))
                .collect();
            if !uncached.is_empty() {
                let (tx, rx) = mpsc::channel();
                self.tex_rx = Some(rx);
                thread::spawn(move || {
                    use rayon::prelude::*;
                    let tx = Arc::new(Mutex::new(tx));
                    let pool = rayon::ThreadPoolBuilder::new()
                        .num_threads(2)
                        .build()
                        .expect("thumbnail pool");
                    pool.install(|| {
                        uncached.par_iter().for_each(|path| {
                            if let Some(ci) = decode_thumbnail(path) {
                                if let Ok(s) = tx.lock() {
                                    let _ = s.send((path.clone(), ci));
                                }
                            }
                        });
                    });
                });
            }
        }

        // Build texture lookup from cache only — always fast, no I/O.
        let textures: Vec<Vec<Option<egui::TextureHandle>>> = groups
            .iter()
            .map(|g| {
                g.images
                    .iter()
                    .map(|img| self.texture_cache.get(&img.path).cloned())
                    .collect()
            })
            .collect();

        let cf = self.settings.clearable_folder.as_deref();
        let clearable_flags: Vec<Vec<bool>> = groups
            .iter()
            .map(|g| g.images.iter().map(|img| is_clearable(&img.path, cf)).collect())
            .collect();

        // Collect pending mutations; apply after rendering.
        let mut to_delete: Vec<PathBuf> = Vec::new();
        let mut to_keep: Vec<PathBuf> = Vec::new();
        let mut open_in_explorer_path: Option<PathBuf> = None;
        let mut open_file_path: Option<PathBuf> = None;
        let mut do_keep_all_best = false;
        let mut do_new_scan = false;
        let mut do_auto_clean_clearable = false;
        let mut start_scan = false;
        let mut start_lf_scan = false;
        let mut go_to_results = false;
        let mut trigger_confirm_auto_clean = false;
        let mut close_confirm_clean = false;
        let mut lf_to_delete: Vec<PathBuf> = Vec::new();
        let mut lf_do_reset = false;
        let mut trigger_pro_modal: Option<&'static str> = None;
        let mut close_pro_modal = false;
        let mut start_cl_analyze = false;
        let mut start_cl_clean = false;
        let mut start_da_analyze = false;
        let mut da_switch_to_dupes: Option<PathBuf> = None;
        let mut ef_delete_all = false;
        let mut ef_delete_one: Option<PathBuf> = None;
        let mut dismiss_update = false;
        let mut open_compare: Option<usize> = None;
        let mut close_compare = false;
        let mut do_export_csv = false;

        CentralPanel::default().show(ctx, |ui| {
            // ── toolbar ────────────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.label(RichText::new("📁").size(18.0));
                ui.add(
                    egui::TextEdit::singleline(&mut self.scan_path)
                        .hint_text("Select a folder to scan…")
                        .desired_width(ui.available_width() - 240.0),
                );
                if ui.button("Browse").clicked() {
                    if let Some(p) = FileDialog::new().set_title("Select Folder").pick_folder() {
                        self.scan_path = p.to_string_lossy().to_string();
                    }
                }
                let is_busy = matches!(self.state, AppState::Scanning)
                    || matches!(self.lf_state, LargeFileState::Scanning)
                    || matches!(self.cl_state, CleanerState::Analyzing | CleanerState::Cleaning)
                    || matches!(self.da_state, DiskAnalyzerState::Scanning);
                let (btn_label, needs_path) = match self.active_tab {
                    AppTab::Duplicates    => ("  Scan  ", true),
                    AppTab::LargeFiles    => ("  Scan  ", true),
                    AppTab::Cleaner       => (" Analyze", false),
                    AppTab::DiskAnalyzer  => ("Analyze Disk", true),
                };
                ui.add_enabled_ui((!needs_path || !self.scan_path.is_empty()) && !is_busy, |ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new(btn_label).color(Color32::WHITE))
                                .fill(Color32::from_rgb(40, 110, 200)),
                        )
                        .clicked()
                    {
                        match self.active_tab {
                            AppTab::Duplicates   => start_scan = true,
                            AppTab::LargeFiles   => start_lf_scan = true,
                            AppTab::Cleaner      => start_cl_analyze = true,
                            AppTab::DiskAnalyzer => start_da_analyze = true,
                        }
                    }
                });
                if ui.button("⚙").on_hover_text("Settings").clicked() {
                    self.show_settings = !self.show_settings;
                }
                if ui.button("?").on_hover_text("About").clicked() {
                    self.show_about = !self.show_about;
                }
                if self.is_pro {
                    ui.label(RichText::new("Pro").small().strong().color(Color32::GOLD));
                } else {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Get Pro").small().color(Color32::BLACK),
                            )
                            .fill(Color32::GOLD),
                        )
                        .on_hover_text("Unlock Auto-Clean, Similar Images, and more")
                        .clicked()
                    {
                        trigger_pro_modal = Some("Pro Features");
                    }
                }
            });

            // ── recent folders ─────────────────────────────────────────────
            if !self.settings.recent_folders.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Recent:").small().color(Color32::from_rgb(90, 90, 90)),
                    );
                    let recents = self.settings.recent_folders.clone();
                    for folder in &recents {
                        let name = std::path::Path::new(folder)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| folder.clone());
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(&name)
                                        .small()
                                        .color(Color32::from_rgb(140, 160, 200)),
                                )
                                .frame(false),
                            )
                            .on_hover_text(folder)
                            .clicked()
                        {
                            self.scan_path = folder.clone();
                        }
                    }
                });
            }

            // ── drag-hover overlay ──────────────────────────────────────────
            if is_drag_hovering {
                let rect = ui.max_rect();
                ui.painter().rect_filled(
                    rect,
                    8.0,
                    Color32::from_rgba_unmultiplied(30, 80, 160, 120),
                );
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new("Drop folder here")
                            .size(28.0)
                            .strong()
                            .color(Color32::WHITE),
                    );
                });
            }

            // ── update banner ──────────────────────────────────────────────
            if let Some(info) = &self.update_info {
                Frame::none()
                    .fill(Color32::from_rgb(30, 60, 30))
                    .inner_margin(egui::style::Margin::symmetric(10.0, 5.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "⬆  {} is available",
                                    info.version
                                ))
                                .color(Color32::from_rgb(120, 220, 120))
                                .strong(),
                            );
                            ui.add_space(8.0);
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("Download →").color(Color32::BLACK).small(),
                                    )
                                    .fill(Color32::from_rgb(80, 200, 80)),
                                )
                                .clicked()
                            {
                                let url = info.release_url.clone();
                                open_url(&url);
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("✕").clicked() {
                                        dismiss_update = true;
                                    }
                                },
                            );
                        });
                    });
            }

            // ── tab bar ────────────────────────────────────────────────────
            ui.horizontal(|ui| {
                let dup_active = matches!(self.active_tab, AppTab::Duplicates);
                let lf_active  = matches!(self.active_tab, AppTab::LargeFiles);
                let cl_active  = matches!(self.active_tab, AppTab::Cleaner);
                let da_active  = matches!(self.active_tab, AppTab::DiskAnalyzer);
                if ui.selectable_label(dup_active, "🔍  Duplicates").clicked() {
                    self.active_tab = AppTab::Duplicates;
                }
                if ui.selectable_label(lf_active, "📦  Large Files").clicked() {
                    self.active_tab = AppTab::LargeFiles;
                }
                if ui.selectable_label(cl_active, "🧹  Cleaner").clicked() {
                    self.active_tab = AppTab::Cleaner;
                }
                if ui.selectable_label(da_active, "💾  Disk Analyzer").clicked() {
                    self.active_tab = AppTab::DiskAnalyzer;
                }
            });

            // ── settings panel ─────────────────────────────────────────────
            if self.show_settings {
                Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut self.settings.include_hidden, "Include hidden folders").changed() {
                            self.settings.save();
                        }
                        ui.separator();
                        let p_resp = ui.add_enabled_ui(self.is_pro, |ui| {
                            ui.checkbox(&mut self.settings.use_perceptual, "Similar images (dHash)")
                        });
                        if p_resp.inner.changed() { self.settings.save(); }
                        if !self.is_pro {
                            ui.label(
                                RichText::new("  (Pro)")
                                    .small()
                                    .color(Color32::from_rgb(200, 160, 40)),
                            );
                        }
                    });
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new("File types:")
                                .small()
                                .color(Color32::from_rgb(120, 120, 120)),
                        );
                        ui.add_enabled_ui(!self.settings.scan_all_files, |ui| {
                            if ui
                                .checkbox(&mut self.settings.scan_documents, "Documents")
                                .on_hover_text("pdf, doc, docx, xls, xlsx, ppt, pptx, txt, …")
                                .changed()
                            {
                                self.settings.save();
                            }
                            if ui
                                .checkbox(&mut self.settings.scan_audio, "Audio")
                                .on_hover_text("mp3, flac, wav, aac, ogg, m4a, …")
                                .changed()
                            {
                                self.settings.save();
                            }
                            let v_resp = ui.add_enabled_ui(self.is_pro, |ui| {
                                ui.checkbox(&mut self.settings.scan_videos, "Videos")
                                    .on_hover_text("mp4, mov, avi, mkv, …")
                            });
                            if v_resp.inner.changed() {
                                self.settings.save();
                            }
                            if !self.is_pro {
                                ui.label(
                                    RichText::new("(Pro)")
                                        .small()
                                        .color(Color32::from_rgb(200, 160, 40)),
                                );
                            }
                            if ui
                                .checkbox(&mut self.settings.scan_archives, "Archives")
                                .on_hover_text("zip, rar, 7z, tar, gz, …")
                                .changed()
                            {
                                self.settings.save();
                            }
                        });
                        if ui.checkbox(&mut self.settings.scan_all_files, "All files").changed() {
                            self.settings.save();
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Custom extensions:")
                                .small()
                                .color(Color32::from_rgb(120, 120, 120)),
                        );
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut self.settings.custom_extensions)
                                    .hint_text("e.g. svg, iso, dmg")
                                    .desired_width(220.0),
                            )
                            .changed()
                        {
                            self.settings.save();
                        }
                    });
                    if self.settings.use_perceptual && self.is_pro {
                        ui.horizontal(|ui| {
                            ui.label("Similarity threshold:");
                            let mut t = self.settings.perceptual_threshold as i64;
                            if ui
                                .add(
                                    egui::DragValue::new(&mut t)
                                        .clamp_range(1_i64..=20_i64)
                                        .suffix(" bits"),
                                )
                                .on_hover_text(
                                    "Max Hamming distance between dHash values (1 = near-identical, 10 = loose match)",
                                )
                                .changed()
                            {
                                self.settings.perceptual_threshold = t as u32;
                                self.settings.save();
                            }
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.label("Large Files threshold:");
                        let mut mb = self.lf_threshold_mb as i64;
                        if ui
                            .add(
                                egui::DragValue::new(&mut mb)
                                    .suffix(" MB")
                                    .clamp_range(1_i64..=100_000_i64),
                            )
                            .changed()
                        {
                            self.lf_threshold_mb = mb as u64;
                            self.settings.large_file_threshold_mb = mb as u64;
                            self.settings.save();
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Clearable folder:");
                        let cf_text =
                            self.settings.clearable_folder.as_deref().unwrap_or("").to_owned();
                        let mut cf_buf = cf_text;
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut cf_buf)
                                    .hint_text("Duplicates here are removed first…")
                                    .desired_width(ui.available_width() - 80.0),
                            )
                            .changed()
                        {
                            self.settings.clearable_folder =
                                if cf_buf.is_empty() { None } else { Some(cf_buf) };
                        }
                        if ui.button("Browse").clicked() {
                            if let Some(p) =
                                FileDialog::new().set_title("Select Clearable Folder").pick_folder()
                            {
                                self.settings.clearable_folder =
                                    Some(p.to_string_lossy().to_string());
                            }
                        }
                        if ui.button("✕").on_hover_text("Clear").clicked() {
                            self.settings.clearable_folder = None;
                        }
                    });
                    ui.label(
                        RichText::new(
                            "When duplicates are found: non-best copies in this folder are trashed; \
                             if this folder has the best copy, the organized copy is trashed and \
                             this file is moved into its place.",
                        )
                        .small()
                        .color(Color32::from_rgb(120, 120, 120)),
                    );
                    if ui
                        .checkbox(&mut self.settings.find_empty_folders, "Find empty folders")
                        .on_hover_text("Also report empty directories after each duplicate scan")
                        .changed()
                    {
                        self.settings.save();
                    }

                    // ── Scheduled scan ────────────────────────────────────
                    ui.separator();
                    ui.label(RichText::new("Scheduled scan").strong());
                    ui.label(
                        RichText::new(
                            "Registers a Windows Task Scheduler job that opens the app \
                             and starts a scan automatically.",
                        )
                        .small()
                        .color(Color32::from_rgb(110, 110, 110)),
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let was_enabled = self.settings.schedule_enabled;
                        if ui
                            .checkbox(&mut self.settings.schedule_enabled, "Enable weekly scan")
                            .changed()
                        {
                            let exe = std::env::current_exe()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();
                            let result = if self.settings.schedule_enabled {
                                scheduler::register(
                                    &exe,
                                    &self.settings.schedule_day.clone(),
                                    self.settings.schedule_hour,
                                )
                            } else {
                                scheduler::unregister()
                            };
                            match result {
                                Ok(()) => { self.schedule_error = None; }
                                Err(e) => {
                                    self.settings.schedule_enabled = was_enabled;
                                    self.schedule_error = Some(e);
                                }
                            }
                            self.settings.save();
                        }
                    });
                    if self.settings.schedule_enabled {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Day:").small());
                            let days = ["MON","TUE","WED","THU","FRI","SAT","SUN"];
                            egui::ComboBox::from_id_source("sched_day")
                                .width(60.0)
                                .selected_text(&self.settings.schedule_day)
                                .show_ui(ui, |ui| {
                                    for &d in &days {
                                        if ui
                                            .selectable_label(
                                                self.settings.schedule_day == d, d,
                                            )
                                            .clicked()
                                            && self.settings.schedule_day != d
                                        {
                                            self.settings.schedule_day = d.to_string();
                                            let exe = std::env::current_exe()
                                                .unwrap_or_default()
                                                .to_string_lossy()
                                                .to_string();
                                            let _ = scheduler::register(
                                                &exe, d, self.settings.schedule_hour,
                                            );
                                            self.settings.save();
                                        }
                                    }
                                });
                            ui.label(RichText::new("at").small());
                            let mut h = self.settings.schedule_hour as i64;
                            if ui
                                .add(
                                    egui::DragValue::new(&mut h)
                                        .clamp_range(0_i64..=23_i64)
                                        .suffix(":00"),
                                )
                                .changed()
                            {
                                self.settings.schedule_hour = h as u8;
                                let exe = std::env::current_exe()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string();
                                let _ = scheduler::register(
                                    &exe,
                                    &self.settings.schedule_day.clone(),
                                    self.settings.schedule_hour,
                                );
                                self.settings.save();
                            }
                        });
                    }
                    if let Some(err) = &self.schedule_error {
                        ui.label(
                            RichText::new(format!("⚠  {err}"))
                                .small()
                                .color(Color32::LIGHT_RED),
                        );
                    }

                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut self.settings.minimize_to_tray, "Minimize to system tray").changed() {
                            // Rebuild or drop the tray icon to match the new preference.
                            self.tray = if self.settings.minimize_to_tray {
                                AppTray::build()
                            } else {
                                None
                            };
                            self.settings.save();
                        }
                    });
                    ui.add_space(4.0);
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
                    ui.label(
                        RichText::new("Settings saved automatically")
                            .small()
                            .color(Color32::from_rgb(80, 80, 80)),
                    );
                    ui.separator();
                    ui.horizontal(|ui| {
                        let log = log_path();
                        ui.label(
                            RichText::new(format!("Debug log: {}", log.display()))
                                .small()
                                .color(Color32::from_rgb(70, 70, 70)),
                        );
                        if ui.small_button("Open").clicked() {
                            let _ = std::process::Command::new("cmd")
                                .args(["/c", "start", "", &log.to_string_lossy()])
                                .spawn();
                        }
                    });
                });
            }

            ui.separator();

            // ── main content ────────────────────────────────────────────────
            match self.active_tab {
                AppTab::Duplicates => match self.state {
                    AppState::Scanning => {
                        ui.add_space(60.0);
                        ui.vertical_centered(|ui| {
                            let paused = self.scan_pause_flag.load(Ordering::Relaxed);
                            ui.label(
                                RichText::new(if paused { "Paused" } else { self.progress.phase })
                                    .size(15.0)
                                    .color(if paused {
                                        Color32::from_rgb(220, 180, 60)
                                    } else {
                                        Color32::GRAY
                                    }),
                            );
                            ui.add_space(10.0);
                            if self.progress.total == 0 {
                                // Indeterminate phase (file collection) — animate the bar
                                ui.add(
                                    ProgressBar::new(0.0)
                                        .animate(!paused)
                                        .text(format!(
                                            "{} files found so far…",
                                            self.progress.processed
                                        ))
                                        .desired_width(420.0),
                                );
                            } else {
                                let frac = self.progress.processed as f32
                                    / self.progress.total as f32;
                                ui.add(
                                    ProgressBar::new(frac)
                                        .text(format!(
                                            "{} / {}",
                                            self.progress.processed, self.progress.total
                                        ))
                                        .desired_width(420.0),
                                );
                            }
                            ui.add_space(12.0);
                            ui.horizontal(|ui| {
                                let btn_w = 90.0 + 8.0 + 80.0;
                                ui.add_space((ui.available_width() - btn_w).max(0.0) / 2.0);
                                let pause_label = if paused { "▶  Resume" } else { "⏸  Pause" };
                                if ui
                                    .add(
                                        egui::Button::new(pause_label)
                                            .min_size(Vec2::new(90.0, 0.0)),
                                    )
                                    .clicked()
                                {
                                    self.scan_pause_flag.store(!paused, Ordering::Relaxed);
                                }
                                ui.add_space(8.0);
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Cancel").color(Color32::WHITE),
                                        )
                                        .fill(Color32::from_rgb(140, 40, 40))
                                        .min_size(Vec2::new(80.0, 0.0)),
                                    )
                                    .clicked()
                                {
                                    self.scan_pause_flag.store(false, Ordering::Relaxed);
                                    self.cancel_flag.store(true, Ordering::Relaxed);
                                }
                            });
                        });
                    }

                    AppState::Summary => {
                        ui.add_space(20.0);
                        ui.vertical_centered(|ui| {
                            let speed = if self.scan_duration_secs > 0.05 {
                                format!(
                                    "{:.0} files/s",
                                    self.scan_file_count as f64 / self.scan_duration_secs
                                )
                            } else {
                                String::new()
                            };
                            ui.label(
                                RichText::new(format!(
                                    "Scan complete — {} files in {:.1}s  {}",
                                    self.scan_file_count,
                                    self.scan_duration_secs,
                                    speed
                                ))
                                .size(13.0)
                                .color(Color32::from_rgb(130, 130, 130)),
                            );
                            ui.add_space(20.0);

                            if self.total_groups == 0 {
                                ui.label(
                                    RichText::new("✓  No duplicates found")
                                        .size(28.0)
                                        .color(Color32::from_rgb(80, 200, 80))
                                        .strong(),
                                );
                                ui.add_space(8.0);
                                ui.label(
                                    RichText::new("All scanned files are unique.")
                                        .size(14.0)
                                        .color(Color32::from_rgb(130, 130, 130)),
                                );
                                ui.add_space(24.0);
                                if ui.button("New Scan").clicked() {
                                    do_new_scan = true;
                                }
                            } else {
                                // Animated "You can free up X" counter — counts up over 1.2s.
                                let anim_t = ctx.animate_value_with_time(
                                    egui::Id::new(("summary_counter", self.scan_id)),
                                    1.0f32,
                                    1.2,
                                );
                                let displayed_wasted =
                                    (self.total_wasted as f64 * anim_t as f64) as u64;

                                Frame::none()
                                    .fill(Color32::from_rgb(25, 60, 25))
                                    .inner_margin(16.0)
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(format!(
                                                "You can free up  {}",
                                                Self::fmt_bytes(displayed_wasted)
                                            ))
                                            .size(28.0)
                                            .color(Color32::from_rgb(100, 240, 100))
                                            .strong(),
                                        );
                                    });

                                ui.add_space(16.0);
                                ui.label(
                                    RichText::new(format!(
                                        "{} duplicate groups  ·  {} redundant files",
                                        self.total_groups, self.total_dup_files
                                    ))
                                    .size(14.0)
                                    .color(Color32::from_rgb(170, 170, 170)),
                                );

                                // ── file type breakdown stacked bar ────────
                                if self.total_wasted > 0 {
                                    ui.add_space(10.0);
                                    let bar_w =
                                        (ui.available_width() * 0.55).min(340.0).max(100.0);
                                    let bar_h = 10.0f32;
                                    let img_frac = (self.image_wasted as f32
                                        / self.total_wasted as f32)
                                        .clamp(0.0, 1.0)
                                        * anim_t;
                                    let vid_frac = (self.video_wasted as f32
                                        / self.total_wasted as f32)
                                        .clamp(0.0, 1.0)
                                        * anim_t;

                                    let (rect, _) = ui.allocate_exact_size(
                                        Vec2::new(bar_w, bar_h),
                                        egui::Sense::hover(),
                                    );
                                    let p = ui.painter();
                                    p.rect_filled(rect, 4.0, Color32::from_rgb(40, 40, 40));
                                    // Image segment (green)
                                    if img_frac > 0.0 {
                                        let r = egui::Rect::from_min_size(
                                            rect.min,
                                            Vec2::new(bar_w * img_frac, bar_h),
                                        );
                                        p.rect_filled(r, 4.0, Color32::from_rgb(60, 160, 60));
                                    }
                                    // Video segment (blue), starts after image segment
                                    if vid_frac > 0.0 {
                                        let x_start = rect.min.x + bar_w * img_frac;
                                        let r = egui::Rect::from_min_size(
                                            egui::Pos2::new(x_start, rect.min.y),
                                            Vec2::new(bar_w * vid_frac, bar_h),
                                        );
                                        p.rect_filled(r, 0.0, Color32::from_rgb(60, 100, 200));
                                    }
                                    ui.add_space(4.0);

                                    // Legend
                                    ui.horizontal(|ui| {
                                        if self.image_wasted > 0 {
                                            ui.label(
                                                RichText::new("■")
                                                    .color(Color32::from_rgb(60, 160, 60))
                                                    .small(),
                                            );
                                            ui.label(
                                                RichText::new(format!(
                                                    "Images {}",
                                                    Self::fmt_bytes(self.image_wasted)
                                                ))
                                                .small()
                                                .color(Color32::from_rgb(130, 130, 130)),
                                            );
                                        }
                                        if self.video_wasted > 0 {
                                            ui.add_space(8.0);
                                            ui.label(
                                                RichText::new("■")
                                                    .color(Color32::from_rgb(60, 100, 200))
                                                    .small(),
                                            );
                                            ui.label(
                                                RichText::new(format!(
                                                    "Videos {}",
                                                    Self::fmt_bytes(self.video_wasted)
                                                ))
                                                .small()
                                                .color(Color32::from_rgb(130, 130, 130)),
                                            );
                                        }
                                    });
                                }

                                // ── disk usage bar ─────────────────────────
                                if self.total_scanned_bytes > 0 {
                                    ui.add_space(20.0);
                                    let bar_w = (ui.available_width() * 0.65).min(380.0).max(100.0);
                                    let bar_h = 14.0;
                                    let frac = (self.total_wasted as f64
                                        / self.total_scanned_bytes as f64)
                                        .min(1.0) as f32;

                                    // "before" bar
                                    ui.label(
                                        RichText::new("Recoverable from scanned files:")
                                            .size(11.0)
                                            .color(Color32::from_rgb(110, 110, 110)),
                                    );
                                    let (rect, _) = ui.allocate_exact_size(
                                        Vec2::new(bar_w, bar_h),
                                        egui::Sense::hover(),
                                    );
                                    let painter = ui.painter();
                                    painter.rect_filled(
                                        rect,
                                        3.0,
                                        Color32::from_rgb(40, 40, 40),
                                    );
                                    if frac > 0.0 {
                                        let fill_rect = egui::Rect::from_min_size(
                                            rect.min,
                                            Vec2::new(bar_w * frac, bar_h),
                                        );
                                        painter.rect_filled(
                                            fill_rect,
                                            3.0,
                                            Color32::from_rgb(200, 80, 40),
                                        );
                                    }
                                    ui.add_space(4.0);
                                    ui.label(
                                        RichText::new(format!(
                                            "{} wasted  of  {} scanned  ({:.1}%)",
                                            Self::fmt_bytes(self.total_wasted),
                                            Self::fmt_bytes(self.total_scanned_bytes),
                                            frac * 100.0,
                                        ))
                                        .size(11.0)
                                        .color(Color32::from_rgb(110, 110, 110)),
                                    );
                                }

                                ui.add_space(24.0);
                                ui.horizontal(|ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new("Review Files →").size(14.0),
                                            )
                                            .min_size(Vec2::new(150.0, 34.0)),
                                        )
                                        .clicked()
                                    {
                                        go_to_results = true;
                                    }
                                    ui.add_space(12.0);
                                    let label = if self.is_pro {
                                        "⚡  Auto-Clean"
                                    } else {
                                        "⚡  Auto-Clean  🔒"
                                    };
                                    let auto_btn = ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(label).size(14.0).color(Color32::BLACK),
                                            )
                                            .fill(Color32::GOLD)
                                            .min_size(Vec2::new(150.0, 34.0)),
                                        )
                                        .on_hover_text(if self.is_pro {
                                            "Keep the best version of each duplicate group"
                                        } else {
                                            "Pro feature — automatically keep best in each group"
                                        });
                                    if auto_btn.clicked() {
                                        if self.is_pro {
                                            trigger_confirm_auto_clean = true;
                                        } else {
                                            trigger_pro_modal = Some("Auto-Clean");
                                        }
                                    }
                                });

                                ui.add_space(24.0);
                                ui.label(
                                    RichText::new("🔒  100% local — files never leave your computer")
                                        .size(11.0)
                                        .color(Color32::from_rgb(90, 90, 90)),
                                );
                            }
                        });
                    }

                    AppState::Results => {
                        // ── summary bar ────────────────────────────────────
                        Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if self.total_groups == 0 {
                                    ui.label(
                                        RichText::new("✓ No duplicates remaining.")
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
                                            RichText::new(format!(
                                                "  ({} in unsorted)",
                                                self.clearable_count
                                            ))
                                            .size(13.0)
                                            .color(Color32::from_rgb(210, 160, 40)),
                                        );
                                    }
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("New scan").clicked() {
                                            do_new_scan = true;
                                        }
                                        if self.total_groups > 0 {
                                            ui.add_space(6.0);
                                            let auto_btn = ui
                                                .add(
                                                    egui::Button::new(if self.is_pro {
                                                        "⚡ Auto-Clean"
                                                    } else {
                                                        "⚡ Auto-Clean 🔒"
                                                    })
                                                    .fill(Color32::GOLD),
                                                )
                                                .on_hover_text(
                                                    "Keep the best version in each group",
                                                );
                                            if auto_btn.clicked() {
                                                if self.is_pro {
                                                    trigger_confirm_auto_clean = true;
                                                } else {
                                                    trigger_pro_modal = Some("Auto-Clean");
                                                }
                                            }
                                            if self.clearable_count > 0 {
                                                ui.add_space(6.0);
                                                let cf_name = self
                                                    .settings
                                                    .clearable_folder
                                                    .as_deref()
                                                    .and_then(|p| {
                                                        std::path::Path::new(p).file_name()
                                                    })
                                                    .map(|n| n.to_string_lossy().to_string())
                                                    .unwrap_or_else(|| "unsorted".to_string());
                                                if ui
                                                    .add(
                                                        egui::Button::new(format!(
                                                            "Auto-clean {}",
                                                            cf_name
                                                        ))
                                                        .fill(Color32::from_rgb(140, 90, 0)),
                                                    )
                                                    .on_hover_text(
                                                        "Delete lower-quality copies from clearable folder.\n\
                                                         If the clearable copy is best, move it to the organized folder.",
                                                    )
                                                    .clicked()
                                                {
                                                    do_auto_clean_clearable = true;
                                                }
                                            }
                                        }
                                    },
                                );
                            });
                            if let Some(msg) = &self.clearable_result {
                                ui.label(
                                    RichText::new(msg)
                                        .small()
                                        .color(Color32::from_rgb(160, 160, 160)),
                                );
                            }
                            if !groups.is_empty() {
                                ui.horizontal(|ui| {
                                    if ui
                                        .small_button("📄  Export CSV")
                                        .on_hover_text("Save all duplicate groups to a CSV file")
                                        .clicked()
                                    {
                                        do_export_csv = true;
                                    }
                                });
                            }
                        });

                        // ── errors ─────────────────────────────────────────
                        if !self.errors.is_empty() {
                            Frame::none()
                                .fill(Color32::from_rgb(70, 15, 15))
                                .inner_margin(6.0)
                                .show(ui, |ui| {
                                    for e in &self.errors {
                                        ui.label(
                                            RichText::new(e).color(Color32::LIGHT_RED).small(),
                                        );
                                    }
                                });
                        }

                        // ── duplicate groups ───────────────────────────────
                        let has_content = !groups.is_empty() || !self.empty_folders.is_empty();
                        if has_content {
                            ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                                for (gi, group) in groups.iter().enumerate() {
                                    let best = group.best_index();
                                    let wasted = Self::fmt_bytes(group.wasted_bytes());

                                    ui.add_space(6.0);
                                    Frame::group(ui.style()).show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            let group_label = if group.is_similar {
                                                format!(
                                                    "≈ {} visually similar files",
                                                    group.images.len()
                                                )
                                            } else {
                                                format!(
                                                    "{} identical files",
                                                    group.images.len()
                                                )
                                            };
                                            ui.label(RichText::new(group_label).strong());
                                            ui.label(
                                                RichText::new(format!("— {} wasted", wasted))
                                                    .color(Color32::GRAY),
                                            );
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if ui
                                                        .small_button("🔎  Compare")
                                                        .on_hover_text("Open side-by-side comparison")
                                                        .clicked()
                                                    {
                                                        open_compare = Some(gi);
                                                    }
                                                    ui.add_space(4.0);
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
                                                        to_keep.push(
                                                            group.images[best].path.clone(),
                                                        );
                                                    }
                                                },
                                            );
                                        });

                                        ui.separator();

                                        ui.horizontal_wrapped(|ui| {
                                            for (ii, img) in group.images.iter().enumerate() {
                                                let is_best = ii == best;
                                                let stroke = if is_best {
                                                    Stroke::new(2.0, Color32::GOLD)
                                                } else {
                                                    Stroke::NONE
                                                };

                                                let card = Frame::none()
                                                    .stroke(stroke)
                                                    .inner_margin(6.0)
                                                    .outer_margin(4.0)
                                                    .show(ui, |ui| {
                                                        ui.set_max_width(230.0);
                                                        ui.vertical(|ui| {
                                                            if let Some(Some(tex)) = textures
                                                                .get(gi)
                                                                .and_then(|t| t.get(ii))
                                                            {
                                                                let sz = tex.size_vec2();
                                                                let aspect = sz.x / sz.y;
                                                                let h = 150.0f32;
                                                                ui.image(
                                                                    tex.id(),
                                                                    Vec2::new(h * aspect, h),
                                                                );
                                                            } else {
                                                                let (icon_text, icon_color) =
                                                                    file_type_icon(&img.path);
                                                                let ext_str = img
                                                                    .path
                                                                    .extension()
                                                                    .and_then(|e| e.to_str())
                                                                    .unwrap_or("")
                                                                    .to_lowercase();
                                                                let (thumb_rect, _) = ui
                                                                    .allocate_exact_size(
                                                                        Vec2::new(150.0, 100.0),
                                                                        egui::Sense::hover(),
                                                                    );
                                                                let painter = ui.painter().clone();
                                                                if is_text_preview_type(&img.path) {
                                                                    let preview = self
                                                                        .text_preview_cache
                                                                        .entry(img.path.clone())
                                                                        .or_insert_with(|| load_text_preview(&img.path))
                                                                        .clone();
                                                                    painter.rect_filled(
                                                                        thumb_rect,
                                                                        6.0,
                                                                        Color32::from_rgb(18, 30, 18),
                                                                    );
                                                                    let origin = thumb_rect.min
                                                                        + Vec2::new(5.0, 5.0);
                                                                    for (i, line) in preview.lines().enumerate() {
                                                                        painter.text(
                                                                            egui::pos2(
                                                                                origin.x,
                                                                                origin.y + i as f32 * 14.0,
                                                                            ),
                                                                            egui::Align2::LEFT_TOP,
                                                                            line,
                                                                            egui::FontId::monospace(9.5),
                                                                            Color32::from_rgb(150, 210, 150),
                                                                        );
                                                                    }
                                                                } else {
                                                                    painter.rect_filled(
                                                                        thumb_rect,
                                                                        6.0,
                                                                        icon_color,
                                                                    );
                                                                    painter.text(
                                                                        thumb_rect.center()
                                                                            - Vec2::new(0.0, 12.0),
                                                                        egui::Align2::CENTER_CENTER,
                                                                        icon_text,
                                                                        egui::FontId::proportional(13.0),
                                                                        Color32::WHITE,
                                                                    );
                                                                    if !ext_str.is_empty() {
                                                                        painter.text(
                                                                            thumb_rect.center()
                                                                                + Vec2::new(0.0, 12.0),
                                                                            egui::Align2::CENTER_CENTER,
                                                                            format!(".{}", ext_str),
                                                                            egui::FontId::proportional(11.0),
                                                                            Color32::from_rgb(200, 200, 200),
                                                                        );
                                                                    }
                                                                }
                                                            }

                                                            if is_best {
                                                                ui.label(
                                                                    RichText::new("★ Best")
                                                                        .color(Color32::GOLD)
                                                                        .small()
                                                                        .strong(),
                                                                );
                                                            }
                                                            if clearable_flags
                                                                .get(gi)
                                                                .and_then(|f| f.get(ii))
                                                                .copied()
                                                                .unwrap_or(false)
                                                            {
                                                                ui.label(
                                                                    RichText::new("📤 Unsorted")
                                                                        .color(Color32::from_rgb(
                                                                            210, 160, 40,
                                                                        ))
                                                                        .small(),
                                                                );
                                                            }

                                                            if img.width > 0 || img.height > 0 {
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
                                                            } else {
                                                                ui.label(
                                                                    RichText::new(Self::fmt_bytes(
                                                                        img.file_size,
                                                                    ))
                                                                    .small()
                                                                    .color(Color32::GRAY),
                                                                );
                                                            }

                                                            let fname = img
                                                                .path
                                                                .file_name()
                                                                .map(|f| {
                                                                    f.to_string_lossy().to_string()
                                                                })
                                                                .unwrap_or_default();
                                                            let folder = img
                                                                .path
                                                                .parent()
                                                                .map(|p| {
                                                                    p.to_string_lossy().to_string()
                                                                })
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
                                                            .on_hover_text(
                                                                img.path.to_string_lossy(),
                                                            );

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
                                                                    .on_hover_text(
                                                                        "Move to Recycle Bin",
                                                                    )
                                                                    .clicked()
                                                                {
                                                                    to_delete.push(img.path.clone());
                                                                }
                                                                if ui
                                                                    .small_button("↗ Open")
                                                                    .on_hover_text("Open with default app")
                                                                    .clicked()
                                                                {
                                                                    open_file_path = Some(img.path.clone());
                                                                }
                                                            });
                                                        });
                                                    });
                                                let card_resp = ui.interact(
                                                    card.response.rect,
                                                    egui::Id::new(("card", gi, ii)),
                                                    egui::Sense::hover(),
                                                );
                                                if card_resp.hovered()
                                                    && ui.input(|i| {
                                                        i.pointer.button_double_clicked(
                                                            egui::PointerButton::Primary,
                                                        )
                                                    })
                                                {
                                                    open_in_explorer_path =
                                                        Some(img.path.clone());
                                                }
                                                card_resp
                                                    .on_hover_text("Double-click to show in Explorer");
                                            }
                                        });
                                    });
                                }

                                // ── empty folders ──────────────────────────
                                if !self.empty_folders.is_empty() {
                                    ui.add_space(10.0);
                                    ui.collapsing(
                                        format!("Empty Folders ({})", self.empty_folders.len()),
                                        |ui| {
                                            ui.add_space(4.0);
                                            if self.is_pro {
                                                if ui
                                                    .add(
                                                        egui::Button::new(
                                                            RichText::new("Delete All")
                                                                .color(Color32::WHITE),
                                                        )
                                                        .fill(Color32::from_rgb(120, 30, 30)),
                                                    )
                                                    .on_hover_text("Permanently remove all empty folders")
                                                    .clicked()
                                                {
                                                    ef_delete_all = true;
                                                }
                                            } else if ui
                                                .add(
                                                    egui::Button::new(
                                                        RichText::new("Delete All 🔒")
                                                            .color(Color32::BLACK),
                                                    )
                                                    .fill(Color32::GOLD),
                                                )
                                                .clicked()
                                            {
                                                trigger_pro_modal = Some("Delete Empty Folders");
                                            }
                                            ui.add_space(4.0);
                                            for folder in &self.empty_folders {
                                                ui.horizontal(|ui| {
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(folder.to_string_lossy())
                                                                .monospace()
                                                                .small()
                                                                .color(Color32::from_rgb(160, 160, 160)),
                                                        )
                                                        .wrap(true),
                                                    );
                                                    if self.is_pro {
                                                        if ui.small_button("Delete").clicked() {
                                                            ef_delete_one = Some(folder.clone());
                                                        }
                                                    } else if ui
                                                        .add(
                                                            egui::Button::new(
                                                                RichText::new("Delete 🔒")
                                                                    .small()
                                                                    .color(Color32::BLACK),
                                                            )
                                                            .fill(Color32::GOLD),
                                                        )
                                                        .clicked()
                                                    {
                                                        trigger_pro_modal =
                                                            Some("Delete Empty Folders");
                                                    }
                                                });
                                            }
                                        },
                                    );
                                }
                            });
                        }
                    }

                    AppState::Idle => {
                        ui.add_space(50.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("Find & Remove Duplicate Files")
                                    .size(22.0)
                                    .strong()
                                    .color(Color32::from_rgb(200, 200, 200)),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("Reclaim disk space in seconds.")
                                    .size(13.0)
                                    .color(Color32::from_rgb(110, 110, 110)),
                            );
                            ui.add_space(28.0);
                            let features: &[(&str, &str, bool)] = &[
                                ("✓", "Exact duplicate detection — by checksum, 100% accurate", false),
                                ("✓", "Similar image matching with dHash", true),
                                ("✓", "Large file finder — see what's eating your disk", false),
                                ("✓", "Video duplicate detection", true),
                                ("✓", "Safe deletion — files go to Recycle Bin", false),
                                ("✓", "100% local — nothing is ever uploaded", false),
                            ];
                            for (icon, text, pro_only) in features {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(*icon)
                                            .size(13.0)
                                            .color(Color32::from_rgb(80, 180, 80)),
                                    );
                                    ui.label(
                                        RichText::new(*text)
                                            .size(13.0)
                                            .color(Color32::from_rgb(160, 160, 160)),
                                    );
                                    if *pro_only && !self.is_pro {
                                        ui.label(
                                            RichText::new("Pro")
                                                .size(10.0)
                                                .color(Color32::from_rgb(200, 160, 40)),
                                        );
                                    }
                                });
                            }
                            ui.add_space(28.0);
                            ui.label(
                                RichText::new("Select a folder above and click  Scan  to start.")
                                    .size(12.0)
                                    .color(Color32::from_rgb(90, 90, 90)),
                            );
                        });
                    }

                    // ── Comparison view ────────────────────────────────────
                    AppState::Comparing(gi) => {
                        let group_opt = groups.get(gi);
                        match group_opt {
                            None => {
                                // Group was deleted — drop back to results.
                                close_compare = true;
                            }
                            Some(group) => {
                                let best = group.best_index();

                                // Header bar
                                Frame::group(ui.style()).show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        if ui.button("← Results").clicked() {
                                            close_compare = true;
                                        }
                                        ui.separator();
                                        let label = if group.is_similar {
                                            format!(
                                                "≈ {} visually similar files — {} reclaimable",
                                                group.images.len(),
                                                Self::fmt_bytes(group.wasted_bytes())
                                            )
                                        } else {
                                            format!(
                                                "{} identical files — {} reclaimable",
                                                group.images.len(),
                                                Self::fmt_bytes(group.wasted_bytes())
                                            )
                                        };
                                        ui.label(RichText::new(label).strong());
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .add(
                                                        egui::Button::new("Keep best")
                                                            .fill(Color32::from_rgb(30, 100, 30)),
                                                    )
                                                    .on_hover_text("Keep highest resolution, delete rest")
                                                    .clicked()
                                                {
                                                    to_keep.push(group.images[best].path.clone());
                                                    close_compare = true;
                                                }
                                            },
                                        );
                                    });
                                });

                                // Large card grid
                                ScrollArea::both().auto_shrink([false; 2]).show(ui, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        for (ii, img) in group.images.iter().enumerate() {
                                            let is_best = ii == best;
                                            let stroke = if is_best {
                                                Stroke::new(3.0, Color32::GOLD)
                                            } else {
                                                Stroke::NONE
                                            };
                                            Frame::none()
                                                .stroke(stroke)
                                                .inner_margin(8.0)
                                                .outer_margin(6.0)
                                                .fill(Color32::from_rgb(22, 22, 22))
                                                .show(ui, |ui| {
                                                    ui.set_max_width(320.0);
                                                    ui.vertical(|ui| {
                                                        // Large thumbnail
                                                        if let Some(Some(tex)) = textures
                                                            .get(gi)
                                                            .and_then(|t| t.get(ii))
                                                        {
                                                            let sz = tex.size_vec2();
                                                            let aspect = sz.x / sz.y;
                                                            let h = 260.0f32;
                                                            ui.image(
                                                                tex.id(),
                                                                Vec2::new(h * aspect, h),
                                                            );
                                                        } else {
                                                            let (icon, color) =
                                                                file_type_icon(&img.path);
                                                            let (r, _) = ui.allocate_exact_size(
                                                                Vec2::new(260.0, 180.0),
                                                                egui::Sense::hover(),
                                                            );
                                                            ui.painter().rect_filled(r, 8.0, color);
                                                            ui.painter().text(
                                                                r.center(),
                                                                egui::Align2::CENTER_CENTER,
                                                                icon,
                                                                egui::FontId::proportional(18.0),
                                                                Color32::WHITE,
                                                            );
                                                        }

                                                        if is_best {
                                                            ui.label(
                                                                RichText::new("★ Best")
                                                                    .color(Color32::GOLD)
                                                                    .strong(),
                                                            );
                                                        }

                                                        // Metadata
                                                        if img.width > 0 || img.height > 0 {
                                                            ui.label(
                                                                RichText::new(format!(
                                                                    "{}×{}  {}",
                                                                    img.width,
                                                                    img.height,
                                                                    Self::fmt_bytes(img.file_size)
                                                                ))
                                                                .size(13.0)
                                                                .color(Color32::from_rgb(200, 200, 200)),
                                                            );
                                                        } else {
                                                            ui.label(
                                                                RichText::new(Self::fmt_bytes(
                                                                    img.file_size,
                                                                ))
                                                                .size(13.0)
                                                                .color(Color32::from_rgb(200, 200, 200)),
                                                            );
                                                        }
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
                                                                    .color(Color32::from_rgb(200, 140, 60)),
                                                            )
                                                            .wrap(true),
                                                        );

                                                        ui.add_space(6.0);
                                                        ui.horizontal(|ui| {
                                                            if ui
                                                                .add(
                                                                    egui::Button::new(
                                                                        RichText::new("Keep")
                                                                            .color(Color32::WHITE),
                                                                    )
                                                                    .fill(Color32::from_rgb(30, 90, 30))
                                                                    .min_size(Vec2::new(80.0, 28.0)),
                                                                )
                                                                .on_hover_text("Delete all others in group")
                                                                .clicked()
                                                            {
                                                                to_keep.push(img.path.clone());
                                                                close_compare = true;
                                                            }
                                                            ui.add_space(6.0);
                                                            if ui
                                                                .add(
                                                                    egui::Button::new(
                                                                        RichText::new("Delete")
                                                                            .color(Color32::WHITE),
                                                                    )
                                                                    .fill(Color32::from_rgb(120, 30, 30))
                                                                    .min_size(Vec2::new(80.0, 28.0)),
                                                                )
                                                                .on_hover_text("Move to Recycle Bin")
                                                                .clicked()
                                                            {
                                                                to_delete.push(img.path.clone());
                                                            }
                                                            if ui
                                                                .small_button("↗ Open")
                                                                .on_hover_text("Open with default app")
                                                                .clicked()
                                                            {
                                                                open_file_path = Some(img.path.clone());
                                                            }
                                                        });
                                                    });
                                                });
                                        }
                                    });
                                });
                            }
                        }
                    }
                },

                AppTab::LargeFiles => match self.lf_state {
                    LargeFileState::Scanning => {
                        ui.add_space(60.0);
                        ui.vertical_centered(|ui| {
                            let paused = self.lf_pause_flag.load(Ordering::Relaxed);
                            ui.label(
                                RichText::new(if paused { "Paused" } else { "Scanning for large files…" })
                                    .size(15.0)
                                    .color(if paused {
                                        Color32::from_rgb(220, 180, 60)
                                    } else {
                                        Color32::GRAY
                                    }),
                            );
                            ui.add_space(10.0);
                            ui.add(ProgressBar::new(0.0).animate(!paused).desired_width(420.0));
                            ui.add_space(12.0);
                            ui.horizontal(|ui| {
                                let btn_w = 90.0 + 8.0 + 80.0;
                                ui.add_space((ui.available_width() - btn_w).max(0.0) / 2.0);
                                let pause_label = if paused { "▶  Resume" } else { "⏸  Pause" };
                                if ui
                                    .add(
                                        egui::Button::new(pause_label)
                                            .min_size(Vec2::new(90.0, 0.0)),
                                    )
                                    .clicked()
                                {
                                    self.lf_pause_flag.store(!paused, Ordering::Relaxed);
                                }
                                ui.add_space(8.0);
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Cancel").color(Color32::WHITE),
                                        )
                                        .fill(Color32::from_rgb(140, 40, 40))
                                        .min_size(Vec2::new(80.0, 0.0)),
                                    )
                                    .clicked()
                                {
                                    self.lf_pause_flag.store(false, Ordering::Relaxed);
                                    self.lf_cancel_flag.store(true, Ordering::Relaxed);
                                }
                            });
                        });
                    }

                    LargeFileState::Results => {
                        let display_limit = if self.is_pro {
                            self.lf_files.len()
                        } else {
                            FREE_LARGE_FILE_LIMIT
                        };
                        let shown_count = display_limit.min(self.lf_files.len());
                        let shown = &self.lf_files[..shown_count];
                        let total_shown_size: u64 = shown.iter().map(|f| f.size).sum();

                        Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{} files  ·  {}",
                                        shown_count,
                                        Self::fmt_bytes(total_shown_size),
                                    ))
                                    .size(13.0),
                                );
                                if !self.is_pro && self.lf_files.len() > FREE_LARGE_FILE_LIMIT {
                                    ui.add_space(8.0);
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(format!(
                                                    "Show all {}  🔒",
                                                    self.lf_files.len()
                                                ))
                                                .small()
                                                .color(Color32::BLACK),
                                            )
                                            .fill(Color32::GOLD),
                                        )
                                        .clicked()
                                    {
                                        trigger_pro_modal = Some("Large Files (unlimited results)");
                                    }
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("New scan").clicked() {
                                            lf_do_reset = true;
                                        }
                                    },
                                );
                            });
                        });

                        ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                            for (idx, entry) in shown.iter().enumerate() {
                                ui.add_space(2.0);
                                let card = Frame::none()
                                    .fill(Color32::from_rgb(25, 25, 25))
                                    .inner_margin(6.0)
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(Self::fmt_bytes(entry.size))
                                                    .strong()
                                                    .color(Color32::from_rgb(220, 180, 60))
                                                    .monospace(),
                                            );
                                            ui.add_space(4.0);
                                            let fname = entry
                                                .path
                                                .file_name()
                                                .map(|f| f.to_string_lossy().to_string())
                                                .unwrap_or_default();
                                            let folder = entry
                                                .path
                                                .parent()
                                                .map(|p| p.to_string_lossy().to_string())
                                                .unwrap_or_default();
                                            ui.vertical(|ui| {
                                                ui.label(RichText::new(&fname).strong());
                                                ui.label(
                                                    RichText::new(&folder)
                                                        .small()
                                                        .color(Color32::GRAY),
                                                );
                                            });
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if ui
                                                        .add(
                                                            egui::Button::new("Delete")
                                                                .fill(Color32::from_rgb(120, 30, 30)),
                                                        )
                                                        .on_hover_text("Move to Recycle Bin")
                                                        .clicked()
                                                    {
                                                        lf_to_delete.push(entry.path.clone());
                                                    }
                                                },
                                            );
                                        });
                                    });
                                let card_resp = ui.interact(
                                    card.response.rect,
                                    egui::Id::new(("lf_card", idx)),
                                    egui::Sense::click(),
                                );
                                if card_resp.double_clicked() {
                                    open_in_explorer_path = Some(entry.path.clone());
                                }
                                card_resp.on_hover_text("Double-click to show in Explorer");
                            }
                        });
                    }

                    LargeFileState::Idle => {
                        ui.add_space(80.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("Find the largest files on your disk")
                                    .size(20.0)
                                    .color(Color32::GRAY),
                            );
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(format!(
                                    "Shows all files larger than {} MB.\n\
                                     Free: top {} results  ·  Pro: unlimited.",
                                    self.lf_threshold_mb, FREE_LARGE_FILE_LIMIT
                                ))
                                .size(12.0)
                                .color(Color32::from_rgb(90, 90, 90)),
                            );
                        });
                    }
                },

                // ── Cleaner tab ───────────────────────────────────────────
                AppTab::Cleaner => {
                    match self.cl_state {
                        CleanerState::Idle => {
                            ui.add_space(60.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new("Remove junk files from your system")
                                        .size(20.0)
                                        .color(Color32::GRAY),
                                );
                                ui.add_space(8.0);
                                ui.label(
                                    RichText::new(
                                        "Click Analyze (top-left) to scan Windows temp files,\n\
                                         browser caches, error reports, and the Recycle Bin.",
                                    )
                                    .size(12.0)
                                    .color(Color32::from_rgb(90, 90, 90)),
                                );
                            });
                        }

                        CleanerState::Analyzing => {
                            ui.add_space(60.0);
                            ui.vertical_centered(|ui| {
                                let paused = self.cl_pause_flag.load(Ordering::Relaxed);
                                let (n, t) = self.cl_progress;
                                ui.label(
                                    RichText::new(if paused { "Paused" } else { "Calculating sizes…" })
                                        .color(if paused {
                                            Color32::from_rgb(220, 180, 60)
                                        } else {
                                            Color32::GRAY
                                        }),
                                );
                                ui.add_space(8.0);
                                let frac = if t > 0 { n as f32 / t as f32 } else { 0.0 };
                                ui.add(ProgressBar::new(frac).animate(!paused).desired_width(300.0));
                                ui.add_space(12.0);
                                ui.horizontal(|ui| {
                                    let btn_w = 90.0 + 8.0 + 80.0;
                                    ui.add_space((ui.available_width() - btn_w).max(0.0) / 2.0);
                                    let pause_label = if paused { "▶  Resume" } else { "⏸  Pause" };
                                    if ui
                                        .add(
                                            egui::Button::new(pause_label)
                                                .min_size(Vec2::new(90.0, 0.0)),
                                        )
                                        .clicked()
                                    {
                                        self.cl_pause_flag.store(!paused, Ordering::Relaxed);
                                    }
                                    ui.add_space(8.0);
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new("Cancel").color(Color32::WHITE),
                                            )
                                            .fill(Color32::from_rgb(140, 40, 40))
                                            .min_size(Vec2::new(80.0, 0.0)),
                                        )
                                        .clicked()
                                    {
                                        self.cl_pause_flag.store(false, Ordering::Relaxed);
                                        self.cl_cancel_flag.store(true, Ordering::Relaxed);
                                    }
                                });
                            });
                        }

                        CleanerState::Cleaning => {
                            ui.add_space(60.0);
                            ui.vertical_centered(|ui| {
                                ui.label(RichText::new("Cleaning…").color(Color32::GRAY));
                                ui.add_space(8.0);
                                ui.add(ProgressBar::new(0.0).animate(true).desired_width(300.0));
                            });
                        }

                        CleanerState::Ready | CleanerState::Done => {
                            ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                                if self.cl_state == CleanerState::Done {
                                    Frame::none()
                                        .fill(Color32::from_rgb(20, 60, 20))
                                        .inner_margin(8.0)
                                        .show(ui, |ui| {
                                            ui.label(
                                                RichText::new(format!(
                                                    "✓  Freed {}",
                                                    Self::fmt_bytes(self.cl_freed_bytes)
                                                ))
                                                .color(Color32::from_rgb(100, 220, 100))
                                                .strong(),
                                            );
                                        });
                                    ui.add_space(6.0);
                                }

                                let mut selected_bytes = 0u64;
                                for cat in &self.cl_categories {
                                    if cat.enabled { selected_bytes += cat.size_bytes; }
                                }

                                Frame::group(ui.style()).show(ui, |ui| {
                                    for cat in &mut self.cl_categories {
                                        ui.horizontal(|ui| {
                                            ui.checkbox(&mut cat.enabled, "");
                                            ui.vertical(|ui| {
                                                ui.label(RichText::new(cat.name).strong());
                                                ui.label(
                                                    RichText::new(cat.description)
                                                        .small()
                                                        .color(Color32::GRAY),
                                                );
                                            });
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if cat.paths.is_empty() && !cat.is_recycle_bin {
                                                        ui.label(
                                                            RichText::new("Not found").color(Color32::GRAY).small(),
                                                        );
                                                    } else {
                                                        ui.label(
                                                            RichText::new(format!(
                                                                "{}  ({} files)",
                                                                Self::fmt_bytes(cat.size_bytes),
                                                                cat.file_count,
                                                            ))
                                                            .monospace()
                                                            .color(Color32::from_rgb(220, 180, 60)),
                                                        );
                                                    }
                                                },
                                            );
                                        });
                                        ui.separator();
                                    }
                                });

                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "Selected: {}",
                                            Self::fmt_bytes(selected_bytes)
                                        ))
                                        .strong(),
                                    );
                                    ui.add_space(16.0);
                                    if self.is_pro {
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new("Clean Selected")
                                                        .color(Color32::WHITE),
                                                )
                                                .fill(Color32::from_rgb(180, 40, 40)),
                                            )
                                            .clicked()
                                        {
                                            start_cl_clean = true;
                                        }
                                    } else {
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    RichText::new("Clean Selected 🔒")
                                                        .color(Color32::BLACK),
                                                )
                                                .fill(Color32::GOLD),
                                            )
                                            .clicked()
                                        {
                                            trigger_pro_modal = Some("Junk Cleaner");
                                        }
                                    }
                                    ui.add_space(8.0);
                                    if ui.small_button("Re-analyze").clicked() {
                                        start_cl_analyze = true;
                                    }
                                });
                            });
                        }
                    }
                }

                // ── Disk Analyzer tab ─────────────────────────────────────
                AppTab::DiskAnalyzer => {
                    match self.da_state {
                        DiskAnalyzerState::Idle => {
                            ui.add_space(60.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new("See what's using your disk space")
                                        .size(20.0)
                                        .color(Color32::GRAY),
                                );
                                ui.add_space(8.0);
                                ui.label(
                                    RichText::new(
                                        "Select a folder and click Analyze Disk to see a\n\
                                         breakdown by file type and the largest subfolders.",
                                    )
                                    .size(12.0)
                                    .color(Color32::from_rgb(90, 90, 90)),
                                );
                            });
                        }

                        DiskAnalyzerState::Scanning => {
                            ui.add_space(60.0);
                            ui.vertical_centered(|ui| {
                                let paused = self.da_pause_flag.load(Ordering::Relaxed);
                                ui.label(
                                    RichText::new(if paused { "Paused" } else { "Scanning disk…" })
                                        .size(15.0)
                                        .color(if paused {
                                            Color32::from_rgb(220, 180, 60)
                                        } else {
                                            Color32::from_rgb(180, 180, 180)
                                        }),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(format!("{} files found", self.da_progress))
                                        .size(13.0)
                                        .color(Color32::GRAY),
                                );
                                ui.add_space(10.0);
                                ui.add(
                                    ProgressBar::new(0.0)
                                        .animate(!paused)
                                        .desired_width(300.0),
                                );
                                ui.add_space(14.0);
                                ui.horizontal(|ui| {
                                    // Manual centering: nudge by half of remaining space.
                                    let btn_w = 90.0 + 8.0 + 80.0;
                                    ui.add_space((ui.available_width() - btn_w).max(0.0) / 2.0);
                                    let pause_label = if paused { "▶  Resume" } else { "⏸  Pause" };
                                    if ui
                                        .add(
                                            egui::Button::new(pause_label)
                                                .min_size(Vec2::new(90.0, 26.0)),
                                        )
                                        .clicked()
                                    {
                                        self.da_pause_flag.store(!paused, Ordering::Relaxed);
                                    }
                                    ui.add_space(8.0);
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new("Cancel").color(Color32::WHITE),
                                            )
                                            .fill(Color32::from_rgb(140, 40, 40))
                                            .min_size(Vec2::new(80.0, 26.0)),
                                        )
                                        .clicked()
                                    {
                                        self.da_pause_flag.store(false, Ordering::Relaxed);
                                        self.da_cancel_flag.store(true, Ordering::Relaxed);
                                    }
                                });
                            });
                        }

                        DiskAnalyzerState::Results => {
                            if let Some(result) = &self.da_result {
                                ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "{}  total in  {}",
                                            Self::fmt_bytes(result.total_size),
                                            result.root.display()
                                        ))
                                        .strong(),
                                    );
                                    ui.add_space(10.0);

                                    // Bar chart
                                    for cat in &result.categories {
                                        if cat.size == 0 { continue; }
                                        let frac = if result.total_size > 0 {
                                            cat.size as f32 / result.total_size as f32
                                        } else {
                                            0.0
                                        };
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(format!("{:10}", cat.name))
                                                    .monospace()
                                                    .size(13.0),
                                            );
                                            let bar_w = 220.0_f32;
                                            let (bar_rect, _) = ui.allocate_exact_size(
                                                Vec2::new(bar_w, 16.0),
                                                egui::Sense::hover(),
                                            );
                                            let p = ui.painter();
                                            p.rect_filled(
                                                bar_rect,
                                                3.0,
                                                Color32::from_rgb(35, 35, 35),
                                            );
                                            let fill = egui::Rect::from_min_size(
                                                bar_rect.min,
                                                Vec2::new(bar_w * frac, bar_rect.height()),
                                            );
                                            p.rect_filled(
                                                fill,
                                                3.0,
                                                Color32::from_rgb(cat.color[0], cat.color[1], cat.color[2]),
                                            );
                                            ui.label(
                                                RichText::new(format!(
                                                    "  {}  ({:.0}%)",
                                                    Self::fmt_bytes(cat.size),
                                                    frac * 100.0
                                                ))
                                                .monospace()
                                                .size(12.0),
                                            );
                                        });
                                    }

                                    ui.add_space(12.0);
                                    ui.separator();
                                    ui.add_space(6.0);
                                    ui.label(RichText::new("Largest subfolders:").strong());
                                    ui.add_space(4.0);

                                    let folders: Vec<_> = result.top_folders.iter().collect();
                                    for entry in folders {
                                        ui.horizontal(|ui| {
                                            let name = entry
                                                .path
                                                .file_name()
                                                .map(|n| n.to_string_lossy().to_string())
                                                .unwrap_or_else(|| entry.path.display().to_string());
                                            ui.label(
                                                RichText::new(&name)
                                                    .monospace()
                                                    .color(Color32::from_rgb(200, 200, 200)),
                                            );
                                            ui.label(
                                                RichText::new(Self::fmt_bytes(entry.size))
                                                    .monospace()
                                                    .color(Color32::from_rgb(220, 180, 60)),
                                            );
                                            if ui
                                                .small_button("→ Scan Dupes")
                                                .on_hover_text("Switch to Duplicates tab and scan this folder")
                                                .clicked()
                                            {
                                                da_switch_to_dupes = Some(entry.path.clone());
                                            }
                                        });
                                    }

                                    ui.add_space(8.0);
                                    if ui.small_button("Re-analyze").clicked() {
                                        start_da_analyze = true;
                                    }
                                });
                            }
                        }
                    }
                }
            }
        });

        // ── Pro upgrade modal ──────────────────────────────────────────────
        if self.show_pro_modal {
            egui::Window::new("Upgrade to Pro")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!(
                            "\"{}\" is a Pro feature",
                            self.pro_modal_feature
                        ))
                        .size(13.0)
                        .color(Color32::from_rgb(200, 160, 40)),
                    );
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(RichText::new("Pro includes:").strong());
                    ui.label("  ⚡  Auto-Clean — keep best, delete the rest");
                    ui.label("  🔍  Similar image detection (dHash)");
                    ui.label("  📦  Large Files — unlimited results");
                    ui.label("  🎬  Video duplicate detection");
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new("One-time purchase  ·  €15")
                            .size(16.0)
                            .strong()
                            .color(Color32::GOLD),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Get Pro →").color(Color32::BLACK).size(14.0),
                                )
                                .fill(Color32::GOLD)
                                .min_size(Vec2::new(110.0, 30.0)),
                            )
                            .clicked()
                        {
                            open_url(PRO_PURCHASE_URL);
                        }
                        ui.add_space(8.0);
                        if ui.button("Maybe later").clicked() {
                            close_pro_modal = true;
                        }
                    });
                    ui.add_space(4.0);
                });
        }

        // ── Confirm Auto-Clean dialog ──────────────────────────────────────
        if self.show_confirm_clean {
            let file_count = self.total_dup_files;
            let bytes = self.total_wasted;
            egui::Window::new("Confirm Auto-Clean")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!(
                            "Delete {} files and free up {}.",
                            file_count,
                            Self::fmt_bytes(bytes)
                        ))
                        .size(14.0),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Files will be moved to the Recycle Bin\nand can be restored if needed.",
                        )
                        .small()
                        .color(Color32::from_rgb(140, 140, 140)),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            close_confirm_clean = true;
                        }
                        ui.add_space(8.0);
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Delete Files").color(Color32::WHITE),
                                )
                                .fill(Color32::from_rgb(180, 40, 40))
                                .min_size(Vec2::new(110.0, 28.0)),
                            )
                            .clicked()
                        {
                            do_keep_all_best = true;
                            go_to_results = true;
                            close_confirm_clean = true;
                        }
                    });
                    ui.add_space(4.0);
                });
        }

        // ── About modal ───────────────────────────────────────────────────────
        if self.show_about {
            egui::Window::new("About")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Disk Cleaner & Duplicate Finder")
                            .size(17.0)
                            .strong(),
                    );
                    ui.label(
                        RichText::new("Find and remove duplicate images, large files, and clutter — in seconds.")
                            .size(12.0)
                            .color(Color32::from_rgb(150, 150, 150)),
                    );
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(6.0);
                    let features: &[&str] = &[
                        "✓  Exact duplicate detection (MD5 checksum)",
                        "✓  Similar image detection (perceptual hash)  — Pro",
                        "✓  Large file finder",
                        "✓  Video duplicate detection  — Pro",
                        "✓  Auto-Clean — keep best, trash the rest  — Pro",
                        "✓  Safe deletion via Recycle Bin",
                        "✓  100% local — nothing ever uploaded",
                    ];
                    for line in features {
                        ui.label(RichText::new(*line).size(12.0).color(Color32::from_rgb(170, 170, 170)));
                    }
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if !self.is_pro {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("Get Pro →").color(Color32::BLACK),
                                    )
                                    .fill(Color32::GOLD),
                                )
                                .clicked()
                            {
                                open_url(PRO_PURCHASE_URL);
                            }
                            ui.add_space(8.0);
                        }
                        if ui.button("Close").clicked() {
                            self.show_about = false;
                        }
                    });
                    ui.add_space(4.0);
                });
        }

        // ── apply mutations after render ───────────────────────────────────
        if start_scan {
            self.start_scan();
        }
        if start_lf_scan {
            self.start_large_file_scan();
        }
        if go_to_results {
            self.state = AppState::Results;
        }
        if do_new_scan {
            self.state = AppState::Idle;
            self.groups.clear();
            self.texture_cache.clear();
            self.text_preview_cache.clear();
            self.tex_rx = None;
            self.errors.clear();
            self.clearable_result = None;
        }
        if do_keep_all_best {
            self.keep_all_best();
        }
        if do_auto_clean_clearable {
            self.auto_clean_clearable();
        }
        if trigger_confirm_auto_clean {
            self.show_confirm_clean = true;
        }
        if close_confirm_clean {
            self.show_confirm_clean = false;
        }
        if let Some(feature) = trigger_pro_modal {
            self.show_pro_modal = true;
            self.pro_modal_feature = feature;
        }
        if close_pro_modal {
            self.show_pro_modal = false;
        }
        for p in to_keep {
            self.keep_image(&p);
        }
        for p in to_delete {
            self.delete_file(&p);
        }
        if lf_do_reset {
            self.lf_state = LargeFileState::Idle;
            self.lf_files.clear();
        }
        for p in lf_to_delete {
            match trash::delete(&p) {
                Ok(()) => self.lf_files.retain(|f| f.path != p),
                Err(e) => self.errors.push(format!("{}: {}", p.display(), e)),
            }
        }
        if let Some(p) = open_in_explorer_path {
            open_in_explorer(&p);
        }
        if let Some(p) = open_file_path {
            open_file(&p);
        }
        if start_cl_analyze {
            self.start_cleaner_analyze();
        }
        if start_cl_clean {
            self.start_cleaner_clean();
        }
        if start_da_analyze {
            self.start_disk_analyze();
        }
        if let Some(p) = da_switch_to_dupes {
            self.scan_path = p.to_string_lossy().to_string();
            self.active_tab = AppTab::Duplicates;
        }
        if ef_delete_all {
            for folder in &self.empty_folders {
                let _ = std::fs::remove_dir_all(folder);
            }
            self.empty_folders.clear();
        }
        if let Some(p) = ef_delete_one {
            if std::fs::remove_dir_all(&p).is_ok() {
                self.empty_folders.retain(|f| f != &p);
            }
        }
        if dismiss_update {
            self.update_info = None;
        }
        if let Some(gi) = open_compare {
            self.state = AppState::Comparing(gi);
        }
        if close_compare {
            self.state = AppState::Results;
        }
        if do_export_csv {
            export_csv(&self.groups);
        }
        // After deletions in Comparing state, validate the group index is still usable.
        if let AppState::Comparing(gi) = self.state {
            if gi >= self.groups.len() || self.groups[gi].images.len() < 2 {
                self.state = AppState::Results;
            }
        }

        if matches!(self.state, AppState::Scanning)
            || matches!(self.lf_state, LargeFileState::Scanning)
            || self.tex_rx.is_some()
            || self.cl_rx.is_some()
            || self.da_rx.is_some()
        {
            ctx.request_repaint();
        }
    }
}
