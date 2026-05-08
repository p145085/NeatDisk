use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::path::PathBuf;

type HmacSha256 = Hmac<Sha256>;

// Must match the LICENSE_SECRET environment variable set in your Cloudflare Worker dashboard.
const SECRET: &[u8] = b"46c2b2aa1835a2e76dd66ab1ca140ec16384e80f60e9f54efa0bbff2f9b96cccfb29370b41c37ed1";

/// Returns true if the key has a valid HMAC-SHA256 signature.
pub fn validate_key(key: &str) -> bool {
    let hex: String = key.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return false;
    }
    let bytes = match hex::decode(&hex) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let rand_bytes = &bytes[0..8];
    let sig_bytes = &bytes[8..16];
    let mut mac = HmacSha256::new_from_slice(SECRET).expect("HMAC accepts any key length");
    mac.update(rand_bytes);
    let result = mac.finalize().into_bytes();
    result[0..8] == *sig_bytes
}

pub fn save_key(key: &str) {
    if let Some(dir) = key_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("license.key"), key.trim());
    }
}

pub fn load_key() -> Option<String> {
    key_dir().and_then(|dir| std::fs::read_to_string(dir.join("license.key")).ok())
}

pub fn is_pro() -> bool {
    load_key()
        .map(|k| validate_key(k.trim()))
        .unwrap_or(false)
}

fn key_dir() -> Option<PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|p| PathBuf::from(p).join("DuplicateFinder"))
}
