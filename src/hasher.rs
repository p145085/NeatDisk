use image::imageops::FilterType;
use std::path::Path;

pub fn exact_hash(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:x}", md5::compute(&bytes)))
}

// dHash: gradient-based 64-bit perceptual hash (used by Pro perceptual matching)
#[allow(dead_code)]
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

#[allow(dead_code)]
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}
