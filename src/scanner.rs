use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::Sender,
    Arc, Mutex,
};
use std::time::Instant;

use rayon::prelude::*;
use walkdir::WalkDir;

use crate::hasher;
use crate::settings::Settings;

pub fn log_path() -> PathBuf {
    std::env::temp_dir().join("dupfinder_debug.log")
}

pub fn clear_log() {
    let _ = std::fs::write(log_path(), "");
}

fn dlog(msg: &str) {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let ts = format!("{}.{:03}", elapsed.as_secs(), elapsed.subsec_millis());
    if let Ok(mut f) =
        std::fs::OpenOptions::new().create(true).append(true).open(log_path())
    {
        let _ = writeln!(f, "{ts}  {msg}");
    }
}

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "avi", "mkv", "webm", "flv", "wmv", "m4v"];
const DOCUMENT_EXTENSIONS: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
    "txt", "rtf", "odt", "ods", "odp", "csv", "md",
];
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "wav", "aac", "ogg", "m4a", "wma", "opus", "aiff",
];
const ARCHIVE_EXTENSIONS: &[&str] = &[
    "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "cab",
];
// O(n²) dHash clustering — cap candidates to stay responsive on large trees.
const MAX_PERCEPTUAL_CANDIDATES: usize = 5_000;
// Files at least this large get a cheap partial-hash pre-filter before full MD5.
const PARTIAL_HASH_THRESHOLD: u64 = 10 * 1024 * 1024; // 10 MB

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
    pub is_similar: bool,
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
    pub total: usize, // 0 = indeterminate (still discovering files)
    pub phase: &'static str,
}

pub struct ScanResult {
    pub groups: Vec<DuplicateGroup>,
    pub file_count: usize,
    pub total_scanned_bytes: u64,
    pub duration_secs: f64,
    pub empty_folders: Vec<PathBuf>,
}

pub enum ScanMessage {
    Progress(ScanProgress),
    Complete(ScanResult),
    Cancelled,
}

/// Sleep-polls until unpaused or cancelled. Returns false if cancelled.
fn wait_while_paused(pause: &AtomicBool, cancel: &AtomicBool) -> bool {
    while pause.load(Ordering::Relaxed) {
        if cancel.load(Ordering::Relaxed) { return false; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    !cancel.load(Ordering::Relaxed)
}

pub fn run_scan(
    root: PathBuf,
    settings: Settings,
    tx: Sender<ScanMessage>,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
) {
    clear_log();
    dlog(&format!("run_scan start  root={}", root.display()));
    let start = Instant::now();

    // Limit parallelism to half the logical CPUs so audio and UI stay responsive.
    let n_threads = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).max(2))
        .unwrap_or(2);
    let scan_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(n_threads)
        .build()
        .expect("scan pool");

    // Phase 0: walk directory and collect paths + sizes in one pass.
    // WalkDir caches entry metadata so this avoids a separate stat() per file.
    // Progress messages use total=0 to signal an indeterminate bar in the UI.
    send_progress(&tx, 0, 0, "Collecting files...");
    let all_files = collect_files(&root, &settings, &tx, &cancel, &pause);
    if cancel.load(Ordering::Relaxed) { tx.send(ScanMessage::Cancelled).ok(); return; }
    let total = all_files.len();
    let total_scanned_bytes: u64 = all_files.iter().map(|(_, sz)| sz).sum();

    // Scan for empty folders in the same root (fast, O(n) walk).
    let empty_folders: Vec<PathBuf> = if settings.find_empty_folders {
        find_empty_folders(&root)
    } else {
        vec![]
    };
    dlog(&format!("phase0 done  total_files={total}  total_bytes={total_scanned_bytes}"));

    if total == 0 {
        tx.send(ScanMessage::Complete(ScanResult {
            groups: vec![],
            file_count: 0,
            total_scanned_bytes: 0,
            duration_secs: start.elapsed().as_secs_f64(),
            empty_folders,
        }))
        .ok();
        return;
    }

    // Phase 1: group by file size — uses pre-collected sizes, no extra stat() calls.
    dlog("phase1 start  grouping by size");
    send_progress(&tx, 0, total, "Grouping by file size...");
    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for (path, size) in &all_files {
        by_size.entry(*size).or_default().push(path.clone());
    }
    by_size.retain(|_, v| v.len() > 1);
    dlog(&format!("phase1 done  size_groups={}", by_size.len()));

    if by_size.is_empty() {
        tx.send(ScanMessage::Complete(ScanResult {
            groups: vec![],
            file_count: total,
            total_scanned_bytes,
            duration_secs: start.elapsed().as_secs_f64(),
            empty_folders,
        }))
        .ok();
        return;
    }
    if !wait_while_paused(&pause, &cancel) { tx.send(ScanMessage::Cancelled).ok(); return; }

    // Phase 1.5: partial hash pre-filter for large files.
    // Reads only first 4 MB + last 4 MB of each large file (≤ 8 MB total I/O per file)
    // to eliminate non-duplicates before committing to full MD5.
    dlog("phase1.5 start  partial-hash pre-filter");
    let mut small_candidates: Vec<PathBuf> = Vec::new();
    let mut large_all: Vec<(PathBuf, u64)> = Vec::new(); // (path, size) for keying

    for (size, paths) in by_size {
        if size < PARTIAL_HASH_THRESHOLD {
            small_candidates.extend(paths);
        } else {
            for p in paths {
                large_all.push((p, size));
            }
        }
    }

    let large_candidates = if large_all.is_empty() {
        Vec::new()
    } else {
        let n_large = large_all.len();
        send_progress(&tx, 0, n_large, "Pre-filtering large files...");
        let counter_lp = Arc::new(AtomicUsize::new(0));
        let tx_lp = Arc::new(Mutex::new(tx.clone()));
        let cancel_lp = cancel.clone();
        let pause_lp = pause.clone();

        let partial_hashed: Vec<(u64, String, PathBuf)> = scan_pool.install(|| {
            large_all
                .par_iter()
                .filter_map(|(path, size)| {
                    while pause_lp.load(Ordering::Relaxed) {
                        if cancel_lp.load(Ordering::Relaxed) { return None; }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    if cancel_lp.load(Ordering::Relaxed) { return None; }
                    let h = hasher::partial_hash(path)?;
                    let n = counter_lp.fetch_add(1, Ordering::Relaxed);
                    if n % 20 == 0 {
                        if let Ok(s) = tx_lp.lock() {
                            s.send(ScanMessage::Progress(ScanProgress {
                                processed: n,
                                total: n_large,
                                phase: "Pre-filtering large files...",
                            }))
                            .ok();
                        }
                    }
                    Some((*size, h, path.clone()))
                })
                .collect()
        });

        // Key by (size, partial_hash) so cross-size files never collide.
        let mut by_partial: HashMap<(u64, String), Vec<PathBuf>> = HashMap::new();
        for (size, hash, path) in partial_hashed {
            by_partial.entry((size, hash)).or_default().push(path);
        }
        by_partial.retain(|_, v| v.len() > 1);
        by_partial.into_values().flatten().collect()
    };

    let candidates: Vec<PathBuf> =
        small_candidates.into_iter().chain(large_candidates).collect();
    let n_candidates = candidates.len();
    dlog(&format!("phase1.5 done  md5_candidates={n_candidates}"));

    if n_candidates == 0 {
        tx.send(ScanMessage::Complete(ScanResult {
            groups: vec![],
            file_count: total,
            total_scanned_bytes,
            duration_secs: start.elapsed().as_secs_f64(),
            empty_folders,
        }))
        .ok();
        return;
    }

    // Phase 2: parallel MD5 of remaining candidates.
    // Wrap Sender in Arc<Mutex> because Sender<T> is Send but not Sync,
    // and rayon closures require Sync for parallel execution.
    if !wait_while_paused(&pause, &cancel) { tx.send(ScanMessage::Cancelled).ok(); return; }
    dlog(&format!("phase2 start  md5  candidates={n_candidates}"));
    send_progress(&tx, 0, n_candidates, "Computing checksums...");
    let counter = Arc::new(AtomicUsize::new(0));
    let tx2 = Arc::new(Mutex::new(tx.clone()));
    let cancel2 = cancel.clone();
    let pause2 = pause.clone();

    let hashed: Vec<(String, PathBuf)> = scan_pool.install(|| {
        candidates
            .par_iter()
            .filter_map(|path| {
                while pause2.load(Ordering::Relaxed) {
                    if cancel2.load(Ordering::Relaxed) { return None; }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                if cancel2.load(Ordering::Relaxed) { return None; }
                let hash = hasher::exact_hash(path)?;
                let n = counter.fetch_add(1, Ordering::Relaxed);
                if n % 50 == 0 {
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
            .collect()
    });

    let mut by_hash: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (hash, path) in hashed {
        by_hash.entry(hash).or_default().push(path);
    }
    by_hash.retain(|_, v| v.len() > 1);
    if !wait_while_paused(&pause, &cancel) { tx.send(ScanMessage::Cancelled).ok(); return; }

    // Phase 3: load dimensions for confirmed exact duplicates only.
    // This is a small set so per-file stat() here is fine.
    let dup_paths: Vec<PathBuf> = by_hash.values().flatten().cloned().collect();
    let n_dups = dup_paths.len();
    dlog(&format!("phase3 start  get_dims  dup_files={n_dups}"));

    send_progress(&tx, 0, n_dups, "Reading file info...");
    let counter2 = Arc::new(AtomicUsize::new(0));
    let tx3 = Arc::new(Mutex::new(tx.clone()));
    let cancel3 = cancel.clone();
    let pause3 = pause.clone();

    let info_map: HashMap<PathBuf, (u32, u32)> = scan_pool.install(|| {
        dup_paths
            .par_iter()
            .filter_map(|path| {
                while pause3.load(Ordering::Relaxed) {
                    if cancel3.load(Ordering::Relaxed) { return None; }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                if cancel3.load(Ordering::Relaxed) { return None; }
                dlog(&format!("  get_dims START  {}", path.display()));
                let dims = get_dims(path);
                dlog(&format!(
                    "  get_dims DONE   {}  -> {:?}",
                    path.display(),
                    dims
                ));
                let dims = dims?;
                let n = counter2.fetch_add(1, Ordering::Relaxed);
                if n % 50 == 0 {
                    if let Ok(s) = tx3.lock() {
                        s.send(ScanMessage::Progress(ScanProgress {
                            processed: n,
                            total: n_dups,
                            phase: "Reading file info...",
                        }))
                        .ok();
                    }
                }
                Some((path.clone(), dims))
            })
            .collect()
    });
    dlog(&format!("phase3 done  dims_found={}", info_map.len()));

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
                Some(DuplicateGroup { images, is_similar: false })
            } else {
                None
            }
        })
        .collect();

    // Most wasted space first.
    groups.sort_by(|a, b| b.wasted_bytes().cmp(&a.wasted_bytes()));

    dlog("phase4 check  perceptual matching");
    // Phase 4: perceptual matching (Pro, opt-in).
    // Runs dHash on files not already in an exact-duplicate group,
    // then clusters by Hamming distance.
    if settings.use_perceptual {
        let exact_paths: HashSet<&PathBuf> =
            groups.iter().flat_map(|g| g.images.iter().map(|img| &img.path)).collect();

        let mut perceptual_candidates: Vec<&PathBuf> = all_files
            .iter()
            .map(|(p, _)| p)
            .filter(|p| !exact_paths.contains(*p) && !is_video(p))
            .collect();

        // Cap to prevent O(n²) clustering from freezing on large trees.
        perceptual_candidates.truncate(MAX_PERCEPTUAL_CANDIDATES);

        let n_perceptual = perceptual_candidates.len();
        send_progress(&tx, 0, n_perceptual, "Computing perceptual hashes...");

        let counter4 = Arc::new(AtomicUsize::new(0));
        let tx4 = Arc::new(Mutex::new(tx.clone()));
        let cancel4 = cancel.clone();
        let pause4 = pause.clone();

        let dhashes: Vec<(u64, PathBuf)> = scan_pool.install(|| {
            perceptual_candidates
                .par_iter()
                .filter_map(|path| {
                    while pause4.load(Ordering::Relaxed) {
                        if cancel4.load(Ordering::Relaxed) { return None; }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    if cancel4.load(Ordering::Relaxed) { return None; }
                    let hash = hasher::dhash(path)?;
                    let n = counter4.fetch_add(1, Ordering::Relaxed);
                    if n % 50 == 0 {
                        if let Ok(s) = tx4.lock() {
                            s.send(ScanMessage::Progress(ScanProgress {
                                processed: n,
                                total: n_perceptual,
                                phase: "Computing perceptual hashes...",
                            }))
                            .ok();
                        }
                    }
                    Some((hash, (*path).clone()))
                })
                .collect()
        });

        let similar = cluster_by_similarity(&dhashes, settings.perceptual_threshold);
        for images in similar {
            groups.push(DuplicateGroup { images, is_similar: true });
        }
    }

    dlog(&format!(
        "run_scan complete  groups={}  elapsed={:.2}s",
        groups.len(),
        start.elapsed().as_secs_f64()
    ));
    tx.send(ScanMessage::Complete(ScanResult {
        groups,
        file_count: total,
        total_scanned_bytes,
        duration_secs: start.elapsed().as_secs_f64(),
        empty_folders,
    }))
    .ok();
}

fn check_custom_ext(ext: &str, custom: &str) -> bool {
    if custom.is_empty() {
        return false;
    }
    custom.split(',').any(|s| {
        let s = s.trim().trim_start_matches('.');
        !s.is_empty() && s.eq_ignore_ascii_case(ext)
    })
}

fn send_progress(tx: &Sender<ScanMessage>, processed: usize, total: usize, phase: &'static str) {
    tx.send(ScanMessage::Progress(ScanProgress { processed, total, phase })).ok();
}

fn get_dims(path: &PathBuf) -> Option<(u32, u32)> {
    // image crate cannot decode video containers; attempting to do so hangs on MP4/WebM.
    if is_video(path) {
        return None;
    }
    let reader = image::io::Reader::open(path).ok()?;
    let reader = reader.with_guessed_format().ok()?;
    reader.into_dimensions().ok()
}

fn is_video(path: &PathBuf) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.iter().any(|v| v.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

// Walks root, yielding (path, file_size) for every matching file.
// Uses WalkDir entry metadata to get sizes without extra stat() calls.
// Sends progress every 500 files with total=0 (indeterminate sentinel).
fn collect_files(
    root: &PathBuf,
    settings: &Settings,
    tx: &Sender<ScanMessage>,
    cancel: &AtomicBool,
    pause: &AtomicBool,
) -> Vec<(PathBuf, u64)> {
    let mut files: Vec<(PathBuf, u64)> = Vec::new();

    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        while pause.load(Ordering::Relaxed) {
            if cancel.load(Ordering::Relaxed) { break; }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if cancel.load(Ordering::Relaxed) { break; }
        if !entry.file_type().is_file() {
            continue;
        }
        if !settings.include_hidden {
            let hidden = entry.path().components().any(|c| {
                let s = c.as_os_str().to_string_lossy();
                s.starts_with('.') || s.starts_with('$')
            });
            if hidden {
                continue;
            }
        }
        let ext_match = if settings.scan_all_files {
            true
        } else {
            match entry.path().extension().and_then(|x| x.to_str()) {
                None => false,
                Some(ext) => {
                    settings.extensions.iter().any(|s| s.eq_ignore_ascii_case(ext))
                        || (settings.scan_videos
                            && VIDEO_EXTENSIONS.iter().any(|v| v.eq_ignore_ascii_case(ext)))
                        || (settings.scan_documents
                            && DOCUMENT_EXTENSIONS.iter().any(|d| d.eq_ignore_ascii_case(ext)))
                        || (settings.scan_audio
                            && AUDIO_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(ext)))
                        || (settings.scan_archives
                            && ARCHIVE_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(ext)))
                        || check_custom_ext(ext, &settings.custom_extensions)
                }
            }
        };
        if !ext_match {
            continue;
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        files.push((entry.path().to_owned(), size));

        if files.len() % 500 == 0 {
            tx.send(ScanMessage::Progress(ScanProgress {
                processed: files.len(),
                total: 0, // indeterminate — total unknown until walk completes
                phase: "Collecting files...",
            }))
            .ok();
        }
    }

    files
}

// Union-find clustering: groups images whose dHash differs by ≤ threshold bits.
fn cluster_by_similarity(hashes: &[(u64, PathBuf)], threshold: u32) -> Vec<Vec<ImageInfo>> {
    let n = hashes.len();
    if n < 2 {
        return vec![];
    }

    let mut parent: Vec<usize> = (0..n).collect();

    for i in 0..n {
        for j in (i + 1)..n {
            if hasher::hamming(hashes[i].0, hashes[j].0) <= threshold {
                let pi = find_root(&mut parent, i);
                let pj = find_root(&mut parent, j);
                if pi != pj {
                    parent[pi] = pj;
                }
            }
        }
    }

    let mut group_map: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = find_root(&mut parent, i);
        group_map.entry(root).or_default().push(i);
    }

    group_map
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|indices| {
            indices
                .into_iter()
                .map(|i| {
                    let path = &hashes[i].1;
                    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    let (width, height) = get_dims(path).unwrap_or((0, 0));
                    ImageInfo { path: path.clone(), file_size, width, height }
                })
                .collect()
        })
        .collect()
}

fn find_root(parent: &mut Vec<usize>, mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]]; // path-halving compression
        x = parent[x];
    }
    x
}

/// Returns all directories under `root` that contain no files anywhere in their subtree.
/// Uses a contents-first walk so child dirs are evaluated before their parents.
pub fn find_empty_folders(root: &Path) -> Vec<PathBuf> {
    use std::collections::HashSet;
    let mut non_empty: HashSet<PathBuf> = HashSet::new();
    let mut empty: Vec<PathBuf> = Vec::new();

    for entry in WalkDir::new(root).contents_first(true).into_iter().flatten() {
        let path = entry.path().to_owned();
        if path == root { continue; }
        if entry.file_type().is_file() {
            if let Some(p) = path.parent() { non_empty.insert(p.to_owned()); }
        } else if entry.file_type().is_dir() {
            if non_empty.contains(&path) {
                if let Some(p) = path.parent() { non_empty.insert(p.to_owned()); }
            } else {
                empty.push(path);
            }
        }
    }
    empty
}
