//! Diagnostics log for the attach flow: what ADE decided for each attach and
//! how the subprocess exited. Best-effort — a log that can't be written is
//! silently skipped, never surfaced to the user.
//!
//! The file is per-user: `$XDG_STATE_HOME/ade/attach.log`, else
//! `~/.local/state/ade/attach.log`, created owner-only. It used to be the
//! fixed path `/tmp/ade-attach.log`, which on a machine with several ADE users
//! was owned by whoever wrote it first: readable by everyone else (session
//! names, SSH targets, the tmux socket path) and, with
//! `fs.protected_regular` set, unwritable for everyone else. There is
//! deliberately no fallback to a shared directory.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

/// Resolve the log path from the two environment values it depends on.
/// Per the XDG spec an unset *or relative* `XDG_STATE_HOME` is ignored in
/// favour of `$HOME/.local/state` (same rule as `mail::mail_dir`). A relative
/// `HOME` is refused too: it would put the log under the process's cwd.
fn resolve(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let state = match xdg_state_home.map(PathBuf::from) {
        Some(p) if p.is_absolute() => p,
        _ => {
            let home = home.map(PathBuf::from).filter(|h| h.is_absolute())?;
            home.join(".local").join("state")
        }
    };
    Some(state.join("ade").join("attach.log"))
}

/// Where this user's attach log lives, or `None` when there is no usable
/// `$XDG_STATE_HOME` / `$HOME`.
pub fn path() -> Option<PathBuf> {
    resolve(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Open the log, creating its directory (0700) and the file (0600) as needed.
/// A symlink at the log path is refused rather than followed, and an existing
/// file is brought back to 0600 — the log names sessions and SSH targets.
fn open(truncate: bool) -> Option<File> {
    let path = path()?;
    let dir = path.parent()?;
    fs::create_dir_all(dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    let mut opts = OpenOptions::new();
    opts.create(true);
    if truncate {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    Some(file)
}

/// Start a fresh log for a new attach (replaces the previous one).
pub fn write(text: &str) {
    if let Some(mut f) = open(true) {
        let _ = f.write_all(text.as_bytes());
    }
}

/// Add to the current attach's log.
pub fn append(text: &str) {
    if let Some(mut f) = open(false) {
        let _ = f.write_all(text.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::resolve;
    use std::path::PathBuf;

    fn os(s: &str) -> Option<std::ffi::OsString> {
        Some(s.into())
    }

    #[test]
    fn absolute_xdg_state_home_wins() {
        assert_eq!(
            resolve(os("/x/state"), os("/home/u")),
            Some(PathBuf::from("/x/state/ade/attach.log"))
        );
    }

    #[test]
    fn relative_or_unset_xdg_falls_back_to_home() {
        let want = Some(PathBuf::from("/home/u/.local/state/ade/attach.log"));
        assert_eq!(resolve(os("rel/state"), os("/home/u")), want);
        assert_eq!(resolve(os(""), os("/home/u")), want);
        assert_eq!(resolve(None, os("/home/u")), want);
    }

    #[test]
    fn no_usable_home_means_no_log_never_a_shared_dir() {
        assert_eq!(resolve(None, None), None);
        assert_eq!(resolve(None, os("relative-home")), None);
        assert_eq!(resolve(os("rel"), None), None);
    }
}
