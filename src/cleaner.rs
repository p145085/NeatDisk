use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use walkdir::WalkDir;

#[derive(Clone)]
pub struct JunkCategory {
    pub name: &'static str,
    pub description: &'static str,
    pub paths: Vec<PathBuf>,
    pub size_bytes: u64,
    pub file_count: usize,
    pub enabled: bool,
    pub is_recycle_bin: bool,
}

pub enum CleanerMsg {
    Progress(usize, usize),
    AnalyzeDone(Vec<JunkCategory>),
    CleanDone { freed_bytes: u64 },
    Cancelled,
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).map(PathBuf::from)
}

fn browser_cache_dirs(user_data: &PathBuf, cache_rel: &[&str]) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(user_data) else { return vec![] };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if n == "Default" || n.starts_with("Profile ") {
            let mut p = entry.path();
            for &seg in cache_rel { p = p.join(seg); }
            if p.exists() { out.push(p); }
        }
    }
    out
}

pub fn resolve_categories() -> Vec<JunkCategory> {
    let mut cats: Vec<JunkCategory> = Vec::new();

    if let Some(p) = env_path("TEMP") {
        if p.exists() {
            cats.push(JunkCategory {
                name: "User Temp Files",
                description: "Temporary files in %TEMP%",
                paths: vec![p],
                size_bytes: 0, file_count: 0, enabled: true, is_recycle_bin: false,
            });
        }
    }

    if let Some(windir) = env_path("WINDIR") {
        let p = windir.join("Temp");
        if p.exists() {
            cats.push(JunkCategory {
                name: "Windows Temp",
                description: "System temporary files in Windows\\Temp",
                paths: vec![p],
                size_bytes: 0, file_count: 0, enabled: true, is_recycle_bin: false,
            });
        }
    }

    if let Some(local) = env_path("LOCALAPPDATA") {
        let base = local.join("Google").join("Chrome").join("User Data");
        let dirs = browser_cache_dirs(&base, &["Cache", "Cache_Data"]);
        if !dirs.is_empty() {
            cats.push(JunkCategory {
                name: "Chrome Cache",
                description: "Google Chrome browser cache",
                paths: dirs,
                size_bytes: 0, file_count: 0, enabled: true, is_recycle_bin: false,
            });
        }

        let base = local.join("Microsoft").join("Edge").join("User Data");
        let dirs = browser_cache_dirs(&base, &["Cache", "Cache_Data"]);
        if !dirs.is_empty() {
            cats.push(JunkCategory {
                name: "Edge Cache",
                description: "Microsoft Edge browser cache",
                paths: dirs,
                size_bytes: 0, file_count: 0, enabled: true, is_recycle_bin: false,
            });
        }
    }

    if let Some(appdata) = env_path("APPDATA") {
        let profiles = appdata.join("Mozilla").join("Firefox").join("Profiles");
        if let Ok(rd) = std::fs::read_dir(&profiles) {
            let dirs: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path().join("cache2").join("entries"))
                .filter(|p| p.exists())
                .collect();
            if !dirs.is_empty() {
                cats.push(JunkCategory {
                    name: "Firefox Cache",
                    description: "Mozilla Firefox browser cache",
                    paths: dirs,
                    size_bytes: 0, file_count: 0, enabled: true, is_recycle_bin: false,
                });
            }
        }

        let wer = appdata.join("Microsoft").join("Windows").join("WER").join("ReportArchive");
        if wer.exists() {
            cats.push(JunkCategory {
                name: "Windows Error Reports",
                description: "Archived Windows crash and error reports",
                paths: vec![wer],
                size_bytes: 0, file_count: 0, enabled: true, is_recycle_bin: false,
            });
        }
    }

    cats.push(JunkCategory {
        name: "Recycle Bin",
        description: "Files waiting in the Recycle Bin",
        paths: vec![],
        size_bytes: 0, file_count: 0, enabled: true, is_recycle_bin: true,
    });

    cats
}

fn walk_size_count(paths: &[PathBuf]) -> (u64, usize) {
    let mut total = 0u64;
    let mut count = 0usize;
    for root in paths {
        for e in WalkDir::new(root).into_iter().flatten() {
            if e.file_type().is_file() {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
                count += 1;
            }
        }
    }
    (total, count)
}

fn recycle_bin_info() -> (u64, usize) {
    use winapi::um::libloaderapi::{GetProcAddress, LoadLibraryA};

    #[repr(C)]
    struct ShQueryRbInfo { cb: u32, size: i64, items: i64 }

    type QueryFn = unsafe extern "system" fn(*const u16, *mut ShQueryRbInfo) -> i32;

    unsafe {
        let shell32 = LoadLibraryA(b"shell32.dll\0".as_ptr() as _);
        if shell32.is_null() { return (0, 0); }
        let ptr = GetProcAddress(shell32, b"SHQueryRecycleBinW\0".as_ptr() as _);
        if ptr.is_null() { return (0, 0); }
        let query: QueryFn = std::mem::transmute(ptr);
        let mut info = ShQueryRbInfo { cb: 24, size: 0, items: 0 };
        query(std::ptr::null(), &mut info);
        (info.size.max(0) as u64, info.items.max(0) as usize)
    }
}

pub fn analyze(
    mut cats: Vec<JunkCategory>,
    tx: Sender<CleanerMsg>,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
) {
    let total = cats.len();
    for (i, cat) in cats.iter_mut().enumerate() {
        loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = tx.send(CleanerMsg::Cancelled);
                return;
            }
            if !pause.load(Ordering::Relaxed) { break; }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = tx.send(CleanerMsg::Progress(i, total));
        if cat.is_recycle_bin {
            let (sz, ct) = recycle_bin_info();
            cat.size_bytes = sz;
            cat.file_count = ct;
        } else {
            let (sz, ct) = walk_size_count(&cat.paths);
            cat.size_bytes = sz;
            cat.file_count = ct;
        }
    }
    let _ = tx.send(CleanerMsg::AnalyzeDone(cats));
}

fn delete_dir_contents(path: &PathBuf) -> u64 {
    let mut freed = 0u64;
    let Ok(rd) = std::fs::read_dir(path) else { return 0 };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_file() {
            let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(&p).is_ok() { freed += sz; }
        } else if p.is_dir() {
            freed += delete_dir_contents(&p);
            let _ = std::fs::remove_dir(&p);
        }
    }
    freed
}

fn empty_recycle_bin() -> u64 {
    use winapi::ctypes::c_void;
    use winapi::um::libloaderapi::{GetProcAddress, LoadLibraryA};

    type EmptyFn = unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32;
    const FLAGS: u32 = 0x0001 | 0x0002 | 0x0004; // NOCONFIRMATION | NOPROGRESSUI | NOSOUND

    let (size_before, _) = recycle_bin_info();
    unsafe {
        let shell32 = LoadLibraryA(b"shell32.dll\0".as_ptr() as _);
        if shell32.is_null() { return 0; }
        let ptr = GetProcAddress(shell32, b"SHEmptyRecycleBinW\0".as_ptr() as _);
        if ptr.is_null() { return 0; }
        let empty: EmptyFn = std::mem::transmute(ptr);
        empty(std::ptr::null_mut(), std::ptr::null(), FLAGS);
    }
    size_before
}

pub fn clean(cats: &[JunkCategory], tx: Sender<CleanerMsg>) {
    let enabled: Vec<&JunkCategory> = cats.iter().filter(|c| c.enabled).collect();
    let total = enabled.len();
    let mut freed = 0u64;
    for (i, cat) in enabled.iter().enumerate() {
        let _ = tx.send(CleanerMsg::Progress(i, total));
        if cat.is_recycle_bin {
            freed += empty_recycle_bin();
        } else {
            for p in &cat.paths {
                freed += delete_dir_contents(p);
            }
        }
    }
    let _ = tx.send(CleanerMsg::CleanDone { freed_bytes: freed });
}
