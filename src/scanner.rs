use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::Sender,
    Arc, Mutex,
};

use rayon::prelude::*;
use walkdir::WalkDir;

use crate::hasher;
use crate::settings::Settings;

#[derive(Clone, Debug)]
pub struct ImageInfo {
    pub path: PathBuf,
    pub file_size: u64,
    pub width: u32,
    pub height: u32,
}

impl ImageInfo {
    pub fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

#[derive(Clone)]
pub struct DuplicateGroup {
    pub images: Vec<ImageInfo>,
}

impl DuplicateGroup {
    pub fn best_index(&self) -> usize {
        self.images
            .iter()
            .enumerate()
            .max_by_key(|(_, img)| (img.pixels(), img.file_size))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    pub fn wasted_bytes(&self) -> u64 {
        let best = self.best_index();
        self.images
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != best)
            .map(|(_, img)| img.file_size)
            .sum()
    }
}

#[derive(Clone)]
pub struct ScanProgress {
    pub processed: usize,
    pub total: usize,
    pub phase: &'static str,
}

pub enum ScanMessage {
    Progress(ScanProgress),
    Complete(Vec<DuplicateGroup>),
}

pub fn run_scan(root: PathBuf, settings: Settings, tx: Sender<ScanMessage>) {
    let all_files = collect_images(&root, &settings);
    let total = all_files.len();

    if total == 0 {
        tx.send(ScanMessage::Complete(vec![])).ok();
        return;
    }

    // Phase 1: group by file size (metadata only — fast)
    send_progress(&tx, 0, total, "Grouping by file size...");

    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for path in &all_files {
        if let Ok(meta) = std::fs::metadata(path) {
            by_size.entry(meta.len()).or_default().push(path.clone());
        }
    }
    by_size.retain(|_, v| v.len() > 1);

    let candidates: Vec<PathBuf> = by_size.into_values().flatten().collect();
    let n_candidates = candidates.len();

    if n_candidates == 0 {
        tx.send(ScanMessage::Complete(vec![])).ok();
        return;
    }

    // Phase 2: parallel MD5 of size-collision candidates
    // Wrap Sender in Arc<Mutex> because Sender<T> is Send but not Sync,
    // and rayon closures require Sync for parallel execution.
    send_progress(&tx, 0, n_candidates, "Computing checksums...");
    let counter = Arc::new(AtomicUsize::new(0));
    let tx2 = Arc::new(Mutex::new(tx.clone()));

    let hashed: Vec<(String, PathBuf)> = candidates
        .par_iter()
        .filter_map(|path| {
            let hash = hasher::exact_hash(path)?;
            let n = counter.fetch_add(1, Ordering::Relaxed);
            if n % 100 == 0 {
                if let Ok(s) = tx2.lock() {
                    s.send(ScanMessage::Progress(ScanProgress {
                        processed: n,
                        total: n_candidates,
                        phase: "Computing checksums...",
                    }))
                    .ok();
                }
            }
            Some((hash, path.clone()))
        })
        .collect();

    let mut by_hash: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (hash, path) in hashed {
        by_hash.entry(hash).or_default().push(path);
    }
    by_hash.retain(|_, v| v.len() > 1);

    // Phase 3: load dimensions for confirmed duplicates only
    let dup_paths: Vec<PathBuf> = by_hash.values().flatten().cloned().collect();
    let n_dups = dup_paths.len();

    send_progress(&tx, 0, n_dups, "Reading image info...");
    let counter2 = Arc::new(AtomicUsize::new(0));
    let tx3 = Arc::new(Mutex::new(tx.clone()));

    let info_map: HashMap<PathBuf, (u32, u32)> = dup_paths
        .par_iter()
        .filter_map(|path| {
            let dims = get_dims(path)?;
            let n = counter2.fetch_add(1, Ordering::Relaxed);
            if n % 50 == 0 {
                if let Ok(s) = tx3.lock() {
                    s.send(ScanMessage::Progress(ScanProgress {
                        processed: n,
                        total: n_dups,
                        phase: "Reading image info...",
                    }))
                    .ok();
                }
            }
            Some((path.clone(), dims))
        })
        .collect();

    let mut groups: Vec<DuplicateGroup> = by_hash
        .into_values()
        .filter_map(|paths| {
            let images: Vec<ImageInfo> = paths
                .into_iter()
                .map(|path| {
                    let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let (width, height) = info_map.get(&path).copied().unwrap_or((0, 0));
                    ImageInfo { path, file_size, width, height }
                })
                .collect();
            if images.len() > 1 {
                Some(DuplicateGroup { images })
            } else {
                None
            }
        })
        .collect();

    // Most wasted space first
    groups.sort_by(|a, b| b.wasted_bytes().cmp(&a.wasted_bytes()));

    tx.send(ScanMessage::Complete(groups)).ok();
}

fn send_progress(tx: &Sender<ScanMessage>, processed: usize, total: usize, phase: &'static str) {
    tx.send(ScanMessage::Progress(ScanProgress { processed, total, phase })).ok();
}

fn get_dims(path: &PathBuf) -> Option<(u32, u32)> {
    let reader = image::io::Reader::open(path).ok()?;
    let reader = reader.with_guessed_format().ok()?;
    reader.into_dimensions().ok()
}

fn collect_images(root: &PathBuf, settings: &Settings) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            settings.include_hidden
                || !e.path().components().any(|c| {
                    let s = c.as_os_str().to_string_lossy();
                    s.starts_with('.') || s.starts_with('$')
                })
        })
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|ext| settings.extensions.iter().any(|s| s.eq_ignore_ascii_case(ext)))
                .unwrap_or(false)
        })
        .map(|e| e.path().to_owned())
        .collect()
}
