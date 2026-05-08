use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct Settings {
    pub include_hidden: bool,
    pub use_perceptual: bool,
    pub perceptual_threshold: u32,
    pub extensions: Vec<String>,
    pub clearable_folder: Option<String>,
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
        }
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
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), json);
        }
    }

    fn path() -> PathBuf {
        std::env::current_exe()
            .unwrap_or_default()
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("settings.json")
    }
}
