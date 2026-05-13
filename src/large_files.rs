use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use walkdir::WalkDir;

#[derive(Clone)]
pub struct LargeFileEntry {
    pub path: PathBuf,
    pub size: u64,
}

pub enum LargeFileMsg {
    Done(Vec<LargeFileEntry>),
    Cancelled,
}

pub fn scan_large_files(
    root: PathBuf,
    min_bytes: u64,
    tx: Sender<LargeFileMsg>,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
) {
    let mut entries: Vec<LargeFileEntry> = Vec::new();

    for e in WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
        while pause.load(Ordering::Relaxed) {
            if cancel.load(Ordering::Relaxed) {
                tx.send(LargeFileMsg::Cancelled).ok();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if cancel.load(Ordering::Relaxed) {
            tx.send(LargeFileMsg::Cancelled).ok();
            return;
        }
        if !e.file_type().is_file() { continue; }
        let path = e.path().to_owned();
        let size = match e.metadata() {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        if size >= min_bytes {
            entries.push(LargeFileEntry { path, size });
        }
    }

    entries.sort_by(|a, b| b.size.cmp(&a.size));
    tx.send(LargeFileMsg::Done(entries)).ok();
}
