//! `ade install-tmux-config` — installs ADE's tmux clipboard config.
//!
//! ADE owns `~/.config/ade/tmux.conf` (canonical body in `MANAGED_BODY`) and
//! adds a single marker'd `source-file` line to `~/.tmux.conf`. The whole
//! install is idempotent and reversible via `--uninstall`.
//!
//! The non-obvious bit is the `terminal-overrides` line:
//!     `set -as terminal-overrides ',*:Ms=\E]52;c%p1%s;%p2%s\007'`
//!
//! Tmux's default `Ms` capability emits OSC 52 with an empty target field
//! (`\E]52;;<base64>\7`), which mosh 1.4.0 silently drops. The override
//! hardcodes the canonical `c` target so mosh forwards the sequence intact.
//! The `%p1%s` is preserved (expands to nothing) because `tparm` requires
//! every `%p` slot tmux passes — drop it and the capability silently emits
//! zero bytes.

use std::fs;
use std::path::PathBuf;

use crate::hosts::{Config, Host};
use crate::ssh_io;

/// Substring used to recognise ADE's source-file line in `~/.tmux.conf`.
pub const MARKER: &str = "ade-tmux-marker";

/// The single line ADE writes into `~/.tmux.conf`. The trailing `# <marker>`
/// is what we grep for on subsequent runs.
const SOURCE_LINE: &str = "source-file -q ~/.config/ade/tmux.conf  # ade-tmux-marker";

/// Canonical contents of `~/.config/ade/tmux.conf`. Bumping `v1` invalidates
/// existing installs (they'll be detected as stale and overwritten on the
/// next `install`).
const MANAGED_BODY: &str = "\
# Managed by ADE — do not edit. Run `ade install-tmux-config --uninstall` to remove.
# ade-tmux-managed v1

# Mouse drag-select-to-copy + OSC 52 clipboard, with the mosh-friendly Ms
# override so the escape sequence survives mosh 1.4.0's strict parser.
set -g mouse on
set -g set-clipboard on
set -as terminal-features ',*:clipboard'
set -as terminal-overrides ',*:Ms=\\E]52;c%p1%s;%p2%s\\007'

bind-key -T copy-mode-vi y send-keys -X copy-pipe-and-cancel
bind-key -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-pipe-and-cancel
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStatus {
    /// Marker absent from `~/.tmux.conf` — never installed (or fully uninstalled).
    Missing,
    /// Marker present and managed file matches canonical body.
    Installed,
    /// Marker present but managed file is missing or has stale content.
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    Created,
    Updated,
    NoOp,
}

#[derive(Debug)]
pub struct InstallReport {
    pub managed_action: FileAction,
    pub conf_action: FileAction,
    pub mouse_off: bool,
}

impl InstallReport {
    pub fn is_noop(&self) -> bool {
        matches!(self.managed_action, FileAction::NoOp)
            && matches!(self.conf_action, FileAction::NoOp)
    }

    pub fn summary(&self) -> String {
        if self.is_noop() {
            return "already installed — nothing to do".to_string();
        }
        let mut parts = Vec::new();
        match self.managed_action {
            FileAction::Created => parts.push("created ~/.config/ade/tmux.conf"),
            FileAction::Updated => parts.push("updated ~/.config/ade/tmux.conf"),
            FileAction::NoOp => {}
        }
        match self.conf_action {
            FileAction::Created => parts.push("created ~/.tmux.conf with source line"),
            FileAction::Updated => parts.push("appended source line to ~/.tmux.conf"),
            FileAction::NoOp => {}
        }
        parts.join("; ")
    }
}

#[derive(Debug)]
pub struct UninstallReport {
    pub managed_removed: bool,
    pub conf_updated: bool,
}

impl UninstallReport {
    pub fn is_noop(&self) -> bool {
        !self.managed_removed && !self.conf_updated
    }

    pub fn summary(&self) -> String {
        if self.is_noop() {
            return "nothing to uninstall — marker absent".to_string();
        }
        let mut parts = Vec::new();
        if self.conf_updated {
            parts.push("removed source line from ~/.tmux.conf");
        }
        if self.managed_removed {
            parts.push("removed ~/.config/ade/tmux.conf");
        }
        parts.join("; ")
    }
}

// ─── Local ──────────────────────────────────────────────────────────────────

pub fn detect_local_status() -> Result<InstallStatus, String> {
    let tmux_conf = home_path(".tmux.conf")?;
    let managed = home_path(".config/ade/tmux.conf")?;
    let has_marker = match fs::read_to_string(&tmux_conf) {
        Ok(body) => contains_marker_line(&body),
        Err(_) => false,
    };
    if !has_marker {
        return Ok(InstallStatus::Missing);
    }
    let managed_ok = matches!(fs::read_to_string(&managed), Ok(body) if body == MANAGED_BODY);
    Ok(if managed_ok {
        InstallStatus::Installed
    } else {
        InstallStatus::Stale
    })
}

pub fn install_local() -> Result<InstallReport, String> {
    let tmux_conf = home_path(".tmux.conf")?;
    let managed = home_path(".config/ade/tmux.conf")?;

    let managed_action = ensure_file_local(&managed, MANAGED_BODY)?;

    let existing_conf = fs::read_to_string(&tmux_conf).unwrap_or_default();
    let conf_action = if contains_marker_line(&existing_conf) {
        FileAction::NoOp
    } else {
        let new_conf = append_source_line(&existing_conf);
        write_atomic(&tmux_conf, &new_conf)?;
        if existing_conf.is_empty() {
            FileAction::Created
        } else {
            FileAction::Updated
        }
    };

    let mouse_off = detect_mouse_off_local();

    Ok(InstallReport {
        managed_action,
        conf_action,
        mouse_off,
    })
}

pub fn uninstall_local() -> Result<UninstallReport, String> {
    let tmux_conf = home_path(".tmux.conf")?;
    let managed = home_path(".config/ade/tmux.conf")?;

    let conf_updated = match fs::read_to_string(&tmux_conf) {
        Ok(body) => {
            let new_body = strip_marker_lines(&body);
            if new_body == body {
                false
            } else {
                write_atomic(&tmux_conf, &new_body)?;
                true
            }
        }
        Err(_) => false,
    };

    let managed_removed = match fs::remove_file(&managed) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("remove {}: {}", managed.display(), e)),
    };

    Ok(UninstallReport {
        managed_removed,
        conf_updated,
    })
}

pub fn detect_mouse_off_local() -> bool {
    let Ok(path) = home_path(".tmux.conf") else {
        return false;
    };
    fs::read_to_string(&path)
        .map(|body| has_mouse_off(&body))
        .unwrap_or(false)
}

// ─── Remote ─────────────────────────────────────────────────────────────────

pub fn detect_remote_status(host: &Host) -> Result<InstallStatus, String> {
    // The whole probe is wrapped in `|| true` so absent files (no
    // `~/.tmux.conf` yet, no managed file yet) yield exit 0 with empty
    // output. Otherwise a fresh-host install errors out before doing any
    // work. The trailing redirect on `cat` keeps stderr quiet either way.
    let cmd = format!(
        "{{ grep -q {marker} ~/.tmux.conf 2>/dev/null && echo MARKER_PRESENT; \
            cat ~/.config/ade/tmux.conf 2>/dev/null; \
         }} || true",
        marker = shell_single_quote(MARKER),
    );
    let out = ssh_io::run(host, &cmd)?;
    let has_marker = out.lines().any(|l| l == "MARKER_PRESENT");
    if !has_marker {
        return Ok(InstallStatus::Missing);
    }
    // Strip the MARKER_PRESENT sentinel before comparing the managed body.
    let body: String = out
        .lines()
        .filter(|l| *l != "MARKER_PRESENT")
        .collect::<Vec<_>>()
        .join("\n");
    // Add trailing newline back so the comparison matches MANAGED_BODY (which
    // ends in a newline).
    let body_with_nl = if body.is_empty() {
        body
    } else {
        format!("{}\n", body)
    };
    Ok(if body_with_nl == MANAGED_BODY {
        InstallStatus::Installed
    } else {
        InstallStatus::Stale
    })
}

pub fn install_remote(config: &Config, host_name: &str) -> Result<InstallReport, String> {
    let host = config
        .host_by_name(host_name)
        .ok_or_else(|| format!("host '{}' not found in config", host_name))?;

    let previous_status = detect_remote_status(host)?;

    // Always (re)write the managed file; remote read+compare costs more than
    // just streaming ~400 bytes.
    let managed_action = match previous_status {
        InstallStatus::Installed => FileAction::NoOp,
        InstallStatus::Stale => {
            ssh_write_managed(host)?;
            FileAction::Updated
        }
        InstallStatus::Missing => {
            ssh_write_managed(host)?;
            FileAction::Created
        }
    };

    let conf_action = match previous_status {
        InstallStatus::Installed | InstallStatus::Stale => FileAction::NoOp,
        InstallStatus::Missing => {
            ssh_append_source_line(host)?;
            FileAction::Updated
        }
    };

    // Remote mouse-off detection is deferred to the user; warning only fires
    // for local installs to keep ssh chatter minimal.
    let mouse_off = false;

    Ok(InstallReport {
        managed_action,
        conf_action,
        mouse_off,
    })
}

pub fn uninstall_remote(
    config: &Config,
    host_name: &str,
) -> Result<UninstallReport, String> {
    let host = config
        .host_by_name(host_name)
        .ok_or_else(|| format!("host '{}' not found in config", host_name))?;

    // Read existing tmux.conf (lenient — file may not exist).
    let existing = ssh_io::run(host, "cat ~/.tmux.conf 2>/dev/null || true")?;
    let stripped = strip_marker_lines(&existing);
    let conf_updated = stripped != existing;
    if conf_updated {
        let cmd = "cat > ~/.tmux.conf.ade-tmp && mv ~/.tmux.conf.ade-tmp ~/.tmux.conf";
        ssh_io::run_with_stdin(host, cmd, stripped.as_bytes())?;
    }

    // Remove the managed file. `rm -f` is idempotent; we report based on a
    // pre-check so the summary reflects reality.
    let pre = ssh_io::run(
        host,
        "test -f ~/.config/ade/tmux.conf && echo Y || true",
    )?;
    let managed_removed = pre.trim() == "Y";
    if managed_removed {
        ssh_io::run(host, "rm -f ~/.config/ade/tmux.conf")?;
    }

    Ok(UninstallReport {
        managed_removed,
        conf_updated,
    })
}

fn ssh_write_managed(host: &Host) -> Result<(), String> {
    let cmd = "mkdir -p ~/.config/ade && cat > ~/.config/ade/tmux.conf.ade-tmp && \
               mv ~/.config/ade/tmux.conf.ade-tmp ~/.config/ade/tmux.conf";
    ssh_io::run_with_stdin(host, cmd, MANAGED_BODY.as_bytes()).map(|_| ())
}

fn ssh_append_source_line(host: &Host) -> Result<(), String> {
    // Append the source line to ~/.tmux.conf, creating it if it doesn't
    // exist. We prefix a leading newline so we don't accidentally fuse onto
    // a previous line that lacks a trailing newline; the redundant blank
    // when the file is empty is harmless.
    let cmd = "touch ~/.tmux.conf && \
               { tail -c1 ~/.tmux.conf | grep -q . && printf '\\n'; cat; printf '\\n'; } \
               >> ~/.tmux.conf";
    ssh_io::run_with_stdin(host, cmd, SOURCE_LINE.as_bytes()).map(|_| ())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn home_path(rel: &str) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "no $HOME set".to_string())?;
    Ok(home.join(rel))
}

fn ensure_file_local(path: &PathBuf, body: &str) -> Result<FileAction, String> {
    match fs::read_to_string(path) {
        Ok(existing) if existing == body => Ok(FileAction::NoOp),
        Ok(_) => {
            write_atomic(path, body)?;
            Ok(FileAction::Updated)
        }
        Err(_) => {
            write_atomic(path, body)?;
            Ok(FileAction::Created)
        }
    }
}

fn write_atomic(path: &PathBuf, body: &str) -> Result<(), String> {
    // Resolve symlinks so dotfiles users (e.g. `~/.tmux.conf` →
    // `~/dotfiles/tmux.conf`) get the real file edited rather than having
    // ADE silently replace the symlink with a regular file. `canonicalize`
    // requires the path to exist, so fall back to the original path for
    // first-time creation.
    let target = path.canonicalize().unwrap_or_else(|_| path.clone());
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create dir {}: {}", parent.display(), e))?;
    }
    let tmp = target.with_extension("ade-tmp");
    fs::write(&tmp, body).map_err(|e| format!("write temp {}: {}", tmp.display(), e))?;
    fs::rename(&tmp, &target).map_err(|e| format!("rename into place: {}", e))?;
    Ok(())
}

fn append_source_line(existing: &str) -> String {
    let mut out = String::with_capacity(existing.len() + SOURCE_LINE.len() + 2);
    out.push_str(existing);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(SOURCE_LINE);
    out.push('\n');
    out
}

fn strip_marker_lines(body: &str) -> String {
    let mut kept: Vec<&str> = body.lines().filter(|l| !is_marker_line(l)).collect();
    // The blank-line spacer we add before the source line (in
    // `append_source_line`) becomes a stray trailing blank after removal.
    // Trim trailing blank lines so uninstall is clean.
    while kept.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        kept.pop();
    }
    if kept.is_empty() {
        String::new()
    } else {
        let mut s = kept.join("\n");
        s.push('\n');
        s
    }
}

fn has_mouse_off(body: &str) -> bool {
    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if (line.starts_with("set ")
            || line.starts_with("set-option ")
            || line.starts_with("setw ")
            || line.starts_with("set-window-option "))
            && line.contains("mouse")
            && line.contains("off")
        {
            return true;
        }
    }
    false
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// True when the line is ADE's `source-file` line (i.e. contains both the
/// marker substring and a `source-file` directive). Tighter than a bare
/// `contains(MARKER)` so a user comment that happens to mention
/// `ade-tmux-marker` doesn't get misidentified as ours.
fn is_marker_line(line: &str) -> bool {
    line.contains(MARKER) && line.contains("source-file")
}

fn contains_marker_line(body: &str) -> bool {
    body.lines().any(is_marker_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_source_line_handles_empty() {
        let out = append_source_line("");
        assert_eq!(out, format!("{}\n", SOURCE_LINE));
    }

    #[test]
    fn append_source_line_separates_with_blank() {
        let out = append_source_line("set -g prefix C-a\n");
        assert!(out.contains("set -g prefix C-a\n\nsource-file"));
    }

    #[test]
    fn append_source_line_adds_trailing_newline_if_missing() {
        let out = append_source_line("set -g prefix C-a");
        assert!(out.contains("set -g prefix C-a\n\nsource-file"));
    }

    #[test]
    fn strip_marker_lines_removes_only_marked() {
        let body = "set -g prefix C-a\nsource-file -q ~/.config/ade/tmux.conf  # ade-tmux-marker\nset -g mouse on\n";
        let out = strip_marker_lines(body);
        assert_eq!(out, "set -g prefix C-a\nset -g mouse on\n");
    }

    #[test]
    fn strip_marker_lines_trims_trailing_blank_spacer() {
        // Mirrors what install + uninstall produce: blank-line spacer +
        // marker line at the end of the file.
        let body = "set -g prefix C-a\n\nsource-file -q ~/.config/ade/tmux.conf  # ade-tmux-marker\n";
        let out = strip_marker_lines(body);
        assert_eq!(out, "set -g prefix C-a\n");
    }

    #[test]
    fn has_mouse_off_detects_set_g() {
        assert!(has_mouse_off("set -g mouse off\n"));
        assert!(has_mouse_off("setw -g mouse off\n"));
        assert!(has_mouse_off("set-option -g mouse off\n"));
    }

    #[test]
    fn has_mouse_off_ignores_comments_and_other_lines() {
        assert!(!has_mouse_off("# set -g mouse off\n"));
        assert!(!has_mouse_off("set -g mouse on\n"));
        assert!(!has_mouse_off(""));
    }

    #[test]
    fn is_marker_line_requires_source_file_directive() {
        // True positive: ADE's actual source line.
        assert!(is_marker_line(SOURCE_LINE));
        // False positive that the old loose check would have hit: a user
        // comment that mentions our marker substring but isn't a source line.
        assert!(!is_marker_line("# why is ade-tmux-marker still here?"));
        // Random non-marker lines.
        assert!(!is_marker_line("set -g mouse on"));
        assert!(!is_marker_line(""));
    }
}
