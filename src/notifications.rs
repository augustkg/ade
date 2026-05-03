//! macOS desktop notifications fired by `App::apply_refresh_result` when a
//! tracked Claude session transitions to "waiting for the user".
//!
//! Why a native API and not OSC 9 escape sequences:
//!
//! - ADE runs inside tmux, and tmux drops passthrough escape sequences from
//!   *invisible* panes (tmux/tmux#3265). The instant the user `Tab`-previews
//!   into another session, ADE's pane becomes invisible — any OSC 9 emitted
//!   from that point would land in /dev/null until the user comes back.
//! - Apple Terminal.app silently ignores OSC 9, so a 1-of-3 supported-terminal
//!   matrix would be misleading.
//! - The macOS notification API is an OS call from ADE's process, with no
//!   rendering path through tmux or any terminal. Banners surface
//!   regardless of which app is focused or which pane is visible.
//!
//! `notify-rust` delegates to `mac-notification-sys` on macOS. Both are
//! actively maintained and ship the `set_application` hook that lets us
//! pick the sender app icon based on `$TERM_PROGRAM`, so clicking the
//! banner focuses the terminal app the user is actually using rather
//! than the default Finder icon.

#[cfg(target_os = "macos")]
use notify_rust::{set_application, Notification};

/// `ADE_NOTIFICATIONS_DRY_RUN=1` causes `fire` to print the banner it
/// *would* have shown to stderr instead of touching the OS. Used by the
/// `ade notifications test` subcommand path and any CI invocation; lets
/// integration tests exercise the dispatch logic without spamming the
/// developer's NotificationCenter.
const DRY_RUN_ENV: &str = "ADE_NOTIFICATIONS_DRY_RUN";

/// Title shown on every banner. Single kind for v1 — a banner means
/// "Claude is waiting for you" whether the trigger was a turn finishing
/// or a permission prompt opening. If the per-kind copy ever needs to
/// diverge, add a `NotificationKind` enum here and split the call sites
/// in `App::apply_refresh_result`.
const TITLE: &str = "Claude is waiting for you";

/// Fire (or simulate) one desktop notification for a session transition.
///
/// `host` / `folder` / `session` are joined into the body as
/// `host/folder/session` (or `host/session` when `folder` is empty), to
/// match the `folder/session | host` shape of the terminal-tab title so
/// the user can correlate the two visually.
///
/// On non-macOS this is a no-op (the build still compiles for Linux dev
/// boxes, but the banner only surfaces on macOS today).
pub fn fire(host: &str, folder: Option<&str>, session: &str) {
    let body = match folder {
        Some(f) if !f.is_empty() => format!("{}/{}/{}", host, f, session),
        _ => format!("{}/{}", host, session),
    };

    if std::env::var(DRY_RUN_ENV).is_ok() {
        eprintln!("[ade-notifications] would fire: {} — {}", TITLE, body);
        return;
    }

    #[cfg(target_os = "macos")]
    {
        // `set_application` requires the bundle identifier of an app
        // already registered with macOS — passing an unknown bundle ID
        // returns an error and `Notification::show` falls back to the
        // default sender. Match the user's terminal so click-to-focus
        // lands them where they expect.
        if let Some(bundle_id) = sender_bundle_id() {
            // Ignore the result — `set_application` errors when the app
            // isn't installed; we just fall back to the crate default.
            let _ = set_application(bundle_id);
        }
        if let Err(e) = Notification::new().summary(TITLE).body(&body).show() {
            eprintln!("[ade-notifications] failed to show banner: {}", e);
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = body; // suppress unused warning on non-macOS builds
    }
}

/// Inspect `$TERM_PROGRAM` to pick the macOS bundle identifier for the
/// terminal hosting ADE. When ADE runs inside tmux, tmux preserves
/// `$TERM_PROGRAM` from the outer terminal, so this still works.
///
/// Returns `None` for unrecognised terminals — `notify-rust` then uses
/// its default sender (`com.apple.Finder`), which still produces a
/// banner but with a generic icon.
#[cfg(target_os = "macos")]
fn sender_bundle_id() -> Option<&'static str> {
    let prog = std::env::var("TERM_PROGRAM").ok()?;
    Some(match prog.as_str() {
        "Apple_Terminal" => "com.apple.Terminal",
        "iTerm.app" => "com.googlecode.iterm2",
        "ghostty" => "com.mitchellh.ghostty",
        "WezTerm" => "com.github.wez.wezterm",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fire_with_folder_and_session_dry_run_does_not_panic() {
        // Dry-run path is the only thing we can exercise without touching
        // the real OS NotificationCenter. Sets the env var, calls fire,
        // restores the env var.
        let prior = std::env::var(DRY_RUN_ENV).ok();
        std::env::set_var(DRY_RUN_ENV, "1");
        fire("local", Some("work"), "web");
        fire("prod", None, "db");
        match prior {
            Some(v) => std::env::set_var(DRY_RUN_ENV, v),
            None => std::env::remove_var(DRY_RUN_ENV),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sender_bundle_id_known_terminals() {
        // Save and restore so we don't pollute the test environment.
        let prior = std::env::var("TERM_PROGRAM").ok();
        for (val, expected) in [
            ("Apple_Terminal", Some("com.apple.Terminal")),
            ("iTerm.app", Some("com.googlecode.iterm2")),
            ("ghostty", Some("com.mitchellh.ghostty")),
            ("WezTerm", Some("com.github.wez.wezterm")),
            ("UnknownTerminal42", None),
        ] {
            std::env::set_var("TERM_PROGRAM", val);
            assert_eq!(sender_bundle_id(), expected, "TERM_PROGRAM={}", val);
        }
        match prior {
            Some(v) => std::env::set_var("TERM_PROGRAM", v),
            None => std::env::remove_var("TERM_PROGRAM"),
        }
    }
}
