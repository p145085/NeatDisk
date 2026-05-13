//! Thin wrapper around `schtasks.exe` for weekly scan registration.
//! No COM or admin rights required — tasks run as the current user.

const TASK_NAME: &str = "NeatDisk Weekly Scan";

pub fn register(exe_path: &str, day: &str, hour: u8) -> Result<(), String> {
    let time = format!("{hour:02}:00");
    // Quote the exe path in case it contains spaces.
    let cmd = format!("\"{exe_path}\" --scheduled-scan");
    let out = std::process::Command::new("schtasks")
        .args([
            "/Create", "/TN", TASK_NAME,
            "/TR", &cmd,
            "/SC", "WEEKLY",
            "/D", day,
            "/ST", &time,
            "/F",   // overwrite if already exists
        ])
        .output()
        .map_err(|e| format!("schtasks: {e}"))?;

    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn unregister() -> Result<(), String> {
    let out = std::process::Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .output()
        .map_err(|e| format!("schtasks: {e}"))?;

    // Exit 1 means "task not found" — treat as success for idempotency.
    if out.status.success() || out.status.code() == Some(1) {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn is_registered() -> bool {
    std::process::Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
