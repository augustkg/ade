//! Append-only file logger gated on the `ADE_LOG` env var. When unset
//! (the default), every call is a no-op. When set to a path, each log
//! line appends `<timestamp> <message>\n` to that file.
//!
//! Used only by the Duplicate-session flow right now — kept narrow so
//! it doesn't accidentally become a full app logger. If a third caller
//! wants in, promote this to a real `log` crate setup.

use std::fs::OpenOptions;
use std::io::Write;

/// Append one line to `$ADE_LOG`. Silently no-ops if the var is unset
/// or the file can't be opened — we never want logging itself to
/// disrupt the foreground flow.
pub fn log(msg: &str) {
    let Ok(path) = std::env::var("ADE_LOG") else {
        return;
    };
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}.{:03}", d.as_secs(), d.subsec_millis()))
        .unwrap_or_else(|_| "?".to_string());
    let _ = writeln!(f, "[{}] {}", ts, msg);
}
