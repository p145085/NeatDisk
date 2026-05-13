use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_large_file_threshold() -> u64 { 50 }
fn default_scan_videos() -> bool { false }
fn default_schedule_day() -> String { "MON".to_string() }
fn default_schedule_hour() -> u8 { 9 }
fn default_minimize_to_tray() -> bool { true }

#[derive(Serialize, Deserialize, Clone)]
pub struct Settings {
    pub include_hidden: bool,
    pub use_perceptual: bool,
    pub perceptual_threshold: u32,
    pub extensions: Vec<String>,
    pub clearable_folder: Option<String>,
    #[serde(default = "default_large_file_threshold")]
    pub large_file_threshold_mb: u64,
    #[serde(default = "default_scan_videos")]
    pub scan_videos: bool,
    #[serde(default)]
    pub recent_folders: Vec<String>,
    #[serde(default)]
    pub scan_documents: bool,
    #[serde(default)]
    pub scan_audio: bool,
    #[serde(default)]
    pub scan_archives: bool,
    #[serde(default)]
    pub scan_all_files: bool,
    #[serde(default)]
    pub custom_extensions: String,
    #[serde(default)]
    pub find_empty_folders: bool,
    #[serde(default)]
    pub schedule_enabled: bool,
    #[serde(default = "default_schedule_day")]
    pub schedule_day: String,
    #[serde(default = "default_schedule_hour")]
    pub schedule_hour: u8,
    #[serde(default = "default_minimize_to_tray")]
    pub minimize_to_tray: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            include_hidden: false,
            use_perceptual: false,
            perceptual_threshold: 5,
            clearable_folder: None,
            extensions: vec![
                "jpg".into(),
                "jpeg".into(),
                "png".into(),
                "gif".into(),
                "bmp".into(),
                "webp".into(),
                "tiff".into(),
                "tif".into(),
            ],
            large_file_threshold_mb: 50,
            scan_videos: false,
            recent_folders: Vec::new(),
            scan_documents: false,
            scan_audio: false,
            scan_archives: false,
            scan_all_files: false,
            custom_extensions: String::new(),
            find_empty_folders: false,
            schedule_enabled: false,
            schedule_day: default_schedule_day(),
            schedule_hour: default_schedule_hour(),
            minimize_to_tray: true,
        }
    }
}

impl Settings {
    pub fn push_recent(&mut self, folder: &str) {
        self.recent_folders.retain(|f| f != folder);
        self.recent_folders.insert(0, folder.to_owned());
        self.recent_folders.truncate(5);
    }
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        Self::ensure_dir();
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), json);
        }
    }

    fn path() -> PathBuf {
        std::env::var("APPDATA")
            .map(|p| PathBuf::from(p).join("NeatDisk").join("settings.json"))
            .unwrap_or_else(|_| PathBuf::from("settings.json"))
    }

    pub fn ensure_dir() {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let _ = std::fs::create_dir_all(PathBuf::from(appdata).join("NeatDisk"));
        }
    }
}
