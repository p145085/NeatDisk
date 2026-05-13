use image::imageops::FilterType;
use std::path::Path;

const PARTIAL_CHUNK: u64 = 4 * 1024 * 1024; // 4 MB per end

// Stream the file through the MD5 context in 1 MB chunks.
// Avoids loading the entire file into memory — critical for large video files.
pub fn exact_hash(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    let mut ctx = md5::Context::new();
    std::io::copy(&mut reader, &mut ctx).ok()?;
    Some(format!("{:x}", ctx.compute()))
}

// Hash first PARTIAL_CHUNK + last PARTIAL_CHUNK bytes only.
// Used as a cheap pre-filter before full MD5 on large files (videos, archives).
// Files > 2×PARTIAL_CHUNK: head read + tail seek+read (≤ 8 MB total I/O).
pub fn partial_hash(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut ctx = md5::Context::new();

    let head_len = PARTIAL_CHUNK.min(size) as usize;
    let mut buf = vec![0u8; head_len];
    file.read_exact(&mut buf).ok()?;
    ctx.consume(&buf);

    if size > PARTIAL_CHUNK * 2 {
        file.seek(SeekFrom::Start(size - PARTIAL_CHUNK)).ok()?;
        let mut tail = vec![0u8; PARTIAL_CHUNK as usize];
        file.read_exact(&mut tail).ok()?;
        ctx.consume(&tail);
    }

    Some(format!("{:x}", ctx.compute()))
}

pub fn dhash(path: &Path) -> Option<u64> {
    let img = image::open(path).ok()?;
    let gray = img.resize_exact(9, 8, FilterType::Nearest).to_luma8();
    let mut hash = 0u64;
    for y in 0..8u32 {
        for x in 0..8u32 {
            if gray.get_pixel(x, y)[0] < gray.get_pixel(x + 1, y)[0] {
                hash |= 1u64 << (y * 8 + x);
            }
        }
    }
    Some(hash)
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}
