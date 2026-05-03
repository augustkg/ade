//! Outer terminal title (OSC 0). ADE writes `\x1b]0;<title>\x07` to stdout
//! whenever the cursor lands on a different session row, so the user's
//! terminal tab acts as a passive "you'll attach to this on Enter" indicator.
//!
//! Scope is deliberately narrow: we only set the *outer* terminal title. We
//! never mutate tmux window or session names. Inside tmux, OSC 0 is captured
//! as the per-pane title (`#T`); for the outer tab to follow it, the user
//! needs `automatic-rename-format '#T'`, which ADE installs via
//! `install-tmux-config`.
//!
//! Sanitization strips ASCII control characters (incl. BEL `\x07`, ESC
//! `\x1b`, the C1 ST `\x9c`, and CR/LF/TAB) so an externally-named tmux
//! session with embedded control bytes can't smuggle a different OSC sequence.
//!
//! Lifecycle around attach: ADE no longer execs into tmux/ssh/mosh —
//! it spawns them as children and waits, with the TUI suspended in
//! between (`tui_lifecycle`). `clear()` fires on suspend so tmux's
//! `set-titles-string` (driven by `@ade-title`) takes over without
//! flashing the ADE row title. After the child exits, `set()` on the
//! next draw re-emits ADE's selection title; the cache flip from the
//! suspend `clear()` ensures that `set` actually writes the bytes
//! rather than no-op'ing.

use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

const MAX_LEN: usize = 200;

fn last_title() -> &'static Mutex<Option<String>> {
    static CELL: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Set the terminal title. Sanitizes and skips redundant writes — calling
/// this every frame is fine.
pub fn set(title: &str) {
    let sanitized = sanitize(title);
    let mut guard = last_title().lock().unwrap();
    if guard.as_deref() == Some(sanitized.as_str()) {
        return;
    }
    let mut stdout = io::stdout().lock();
    let _ = write!(stdout, "\x1b]0;{}\x07", sanitized);
    let _ = stdout.flush();
    *guard = Some(sanitized);
}

/// Emit an empty OSC 0 so the terminal falls back to its own default
/// (typically the foreground command). Use right before `exec_replace` and
/// right after `ratatui::restore()` so the user never sees a stale ADE
/// title.
pub fn clear() {
    set("");
}

/// Strip control chars and truncate. Truncation is byte-bounded but only at
/// char boundaries — never splits a multi-byte UTF-8 sequence.
pub fn sanitize(input: &str) -> String {
    let mut out = String::with_capacity(input.len().min(MAX_LEN));
    let mut len = 0;
    for c in input.chars() {
        if c.is_control() {
            continue;
        }
        let cl = c.len_utf8();
        if len + cl > MAX_LEN {
            break;
        }
        out.push(c);
        len += cl;
    }
    out
}

/// Format a session-row title. Loose sessions (no folder prefix) collapse to
/// `<leaf> | <host>` rather than carrying a `*` placeholder.
pub fn format_session(prefix: Option<&str>, leaf: &str, host: &str) -> String {
    match prefix {
        Some(p) => format!("{}/{} | {}", p, leaf, host),
        None => format!("{} | {}", leaf, host),
    }
}

/// Compute the title for a tmux session by name + host label. Splits `name`
/// on the first `/` (matching `model::Session::from_tmux`) so the title
/// reads the same as what the in-TUI cursor shows.
pub fn for_session_name(name: &str, host: &str) -> String {
    let (prefix, leaf) = match name.split_once('/') {
        Some((p, l)) if !p.is_empty() && !l.is_empty() => (Some(p), l),
        _ => (None, name),
    };
    format_session(prefix, leaf, host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_bel_esc_st_newline_tab() {
        let raw = "ok\x07\x1b\u{9c}\r\n\there";
        assert_eq!(sanitize(raw), "okhere");
    }

    #[test]
    fn sanitize_preserves_unicode() {
        assert_eq!(sanitize("föo · bar 🚀"), "föo · bar 🚀");
    }

    #[test]
    fn sanitize_truncates_at_char_boundary() {
        // 100 copies of a 4-byte char = 400 bytes, but MAX_LEN is 200 → 50
        // chars fit, the 51st would push us to 204 > 200 so it's dropped.
        let s: String = "🚀".repeat(100);
        let out = sanitize(&s);
        assert_eq!(out.chars().count(), 50);
        assert!(out.len() <= MAX_LEN);
    }

    #[test]
    fn sanitize_truncates_ascii() {
        let s = "x".repeat(300);
        assert_eq!(sanitize(&s).len(), MAX_LEN);
    }

    #[test]
    fn format_session_with_prefix() {
        assert_eq!(
            format_session(Some("work"), "web", "local"),
            "work/web | local"
        );
    }

    #[test]
    fn format_session_loose_drops_slash() {
        assert_eq!(format_session(None, "scratch", "local"), "scratch | local");
    }

    #[test]
    fn for_session_name_splits_first_slash() {
        assert_eq!(
            for_session_name("work/web", "local"),
            "work/web | local"
        );
    }

    #[test]
    fn for_session_name_loose_no_slash() {
        assert_eq!(for_session_name("scratch", "local"), "scratch | local");
    }

    #[test]
    fn for_session_name_treats_leading_slash_as_loose() {
        // "/foo" has empty prefix → falls through to loose path.
        assert_eq!(for_session_name("/foo", "local"), "/foo | local");
    }
}
