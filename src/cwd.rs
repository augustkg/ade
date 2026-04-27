use std::path::PathBuf;

/// Guess a folder/prefix label from the current working directory.
/// Returns the basename of `$PWD` (e.g. `/Users/foo/Dev/ADE` → `Some("ADE")`).
/// Returns `None` if cwd is the user's home directory or root.
pub fn guess_prefix() -> Option<String> {
    let pwd = std::env::current_dir().ok()?;
    if let Some(home) = home_dir() {
        if pwd == home {
            return None;
        }
    }
    let basename = pwd.file_name()?.to_string_lossy().to_string();
    if basename.is_empty() {
        None
    } else {
        Some(sanitize(&basename))
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}
