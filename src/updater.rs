use std::sync::mpsc::Sender;

const RELEASES_API: &str =
    "https://api.github.com/repos/p145085/NeatDisk/releases/latest";

pub struct UpdateInfo {
    pub version: String,
    pub release_url: String,
}

/// Spawns a background thread that checks for a newer GitHub release and sends
/// the result through `tx`.  Never blocks the calling thread.
pub fn spawn_check(tx: Sender<Option<UpdateInfo>>) {
    std::thread::spawn(move || {
        let _ = tx.send(check());
    });
}

fn check() -> Option<UpdateInfo> {
    let body = ureq::get(RELEASES_API)
        .set("User-Agent", concat!("NeatDisk/", env!("CARGO_PKG_VERSION")))
        .call()
        .ok()?
        .into_string()
        .ok()?;

    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = json["tag_name"].as_str()?;
    let url = json["html_url"].as_str()?;

    if is_newer(tag.trim_start_matches('v'), env!("CARGO_PKG_VERSION")) {
        Some(UpdateInfo {
            version: tag.to_string(),
            release_url: url.to_string(),
        })
    } else {
        None
    }
}

fn is_newer(remote: &str, current: &str) -> bool {
    parse_semver(remote) > parse_semver(current)
}

fn parse_semver(s: &str) -> (u32, u32, u32) {
    let mut it = s.split('.').filter_map(|p| p.parse::<u32>().ok());
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}
