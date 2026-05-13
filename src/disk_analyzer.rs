use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use jwalk::WalkDir;

pub struct FileCategory {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub color: [u8; 3],
    pub size: u64,
}

pub struct FolderEntry {
    pub path: PathBuf,
    pub size: u64,
}

pub struct DiskAnalysis {
    pub root: PathBuf,
    pub total_size: u64,
    pub categories: Vec<FileCategory>,
    pub top_folders: Vec<FolderEntry>,
}

pub enum DiskMsg {
    Progress(usize),
    Done(DiskAnalysis),
    Cancelled,
}

static CATEGORIES: &[(&str, &[&str], [u8; 3])] = &[
    ("Images",    &["jpg","jpeg","png","gif","bmp","webp","tiff","tif","heic","raw","cr2","nef","arw","dng"],  [70, 130, 200]),
    ("Videos",    &["mp4","mov","avi","mkv","webm","flv","wmv","m4v","mpg","mpeg"],                            [210, 100, 50]),
    ("Audio",     &["mp3","flac","wav","aac","ogg","m4a","wma","opus","aiff"],                                 [150, 80, 200]),
    ("Documents", &["pdf","doc","docx","xls","xlsx","ppt","pptx","txt","rtf","odt","csv","md"],                [80, 180, 80]),
    ("Archives",  &["zip","rar","7z","tar","gz","bz2","xz","zst","cab","iso"],                                 [180, 150, 60]),
];

pub fn analyze(root: PathBuf, tx: Sender<DiskMsg>, cancel: Arc<AtomicBool>, pause: Arc<AtomicBool>) {
    let mut categories: Vec<FileCategory> = CATEGORIES
        .iter()
        .map(|(name, exts, color)| FileCategory { name, extensions: exts, color: *color, size: 0 })
        .collect();
    categories.push(FileCategory { name: "Other", extensions: &[], color: [100, 100, 100], size: 0 });

    let mut folder_sizes: HashMap<PathBuf, u64> = HashMap::new();
    // Cache top-level children that are directories to avoid a stat call per file.
    let mut dir_cache: HashSet<PathBuf> = HashSet::new();
    let mut total_size = 0u64;
    let mut file_count = 0usize;

    // jwalk parallelises the directory-listing phase (ReadDir) across a rayon pool,
    // which is the I/O bottleneck. Sequential accumulation in the consumer loop is fine.
    let threads = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).max(2).min(8))
        .unwrap_or(4);

    for entry in WalkDir::new(&root)
        .parallelism(jwalk::Parallelism::RayonNewPool(threads))
        .into_iter()
        .flatten()
    {
        // Honour pause — sleep until resumed or cancelled.
        while pause.load(Ordering::Relaxed) {
            if cancel.load(Ordering::Relaxed) {
                let _ = tx.send(DiskMsg::Cancelled);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = tx.send(DiskMsg::Cancelled);
            return;
        }
        if !entry.file_type().is_file() { continue; }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        total_size += size;
        file_count += 1;

        if file_count % 200 == 0 {
            let _ = tx.send(DiskMsg::Progress(file_count));
        }

        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let mut matched = false;
        for cat in categories.iter_mut().take(CATEGORIES.len()) {
            if cat.extensions.iter().any(|&e| e.eq_ignore_ascii_case(ext)) {
                cat.size += size;
                matched = true;
                break;
            }
        }
        if !matched {
            categories.last_mut().unwrap().size += size;
        }

        // Accumulate size into the top-level subfolder bucket.
        if let Ok(rel) = path.strip_prefix(&root) {
            if let Some(first) = rel.components().next() {
                let top = root.join(first);
                let is_dir = if dir_cache.contains(&top) {
                    true
                } else if top.is_dir() {
                    dir_cache.insert(top.clone());
                    true
                } else {
                    false
                };
                if is_dir {
                    *folder_sizes.entry(top).or_insert(0) += size;
                }
            }
        }
    }

    let mut top_folders: Vec<FolderEntry> = folder_sizes
        .into_iter()
        .map(|(path, size)| FolderEntry { path, size })
        .collect();
    top_folders.sort_by(|a, b| b.size.cmp(&a.size));
    top_folders.truncate(25);

    let _ = tx.send(DiskMsg::Done(DiskAnalysis { root, total_size, categories, top_folders }));
}
