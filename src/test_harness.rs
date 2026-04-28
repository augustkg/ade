//! Test-only helpers used by the embedded-terminal acceptance tests.
//!
//! The big move is `IsolatedTmux`: spins up a tmux server on a unique
//! socket name (so it can't contaminate the user's real tmux server or
//! a parallel `cargo test`), then exposes a small API for creating /
//! inspecting / capturing test sessions. `Drop` reliably kills the
//! server, even on panic.
//!
//! Polling helpers (`poll_until`) replace fixed sleeps so tests aren't
//! flaky on slow machines; they fail loudly on timeout instead of
//! racing.

#![cfg(test)]
#![allow(dead_code)] // populated incrementally across phases

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Acceptance tests mutate process-wide env (`TMUX_TMPDIR`, `HOME`,
/// `XDG_CONFIG_HOME`) so they MUST run serially within a single
/// `cargo test` invocation. Each acceptance test holds this mutex for
/// the duration of its setup + assertions. Unit tests don't need it.
///
/// **Footgun warning** (Codex Phase-9 review): the env mutation is
/// process-global. If a future *unit* test reads `HOME` /
/// `XDG_CONFIG_HOME` / `TMUX_TMPDIR` (e.g. anything that calls
/// `Config::load`, `State::load`, or shells out to plain `tmux`),
/// it must also acquire this lock or it can observe acceptance-test
/// fixture values. Today no unit tests touch those vars, but the
/// rule needs to hold.
static ACCEPTANCE_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the acceptance-mode env lock. Returned guard drops at the
/// end of the test, releasing the lock for the next one. Use this
/// before constructing an `IsolatedTmux`.
pub(crate) fn acquire_acceptance_lock() -> MutexGuard<'static, ()> {
    // poisoned mutex from a panicking test is still usable for us;
    // we don't share state, so consume the poison.
    ACCEPTANCE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A live tmux server on an isolated socket-dir, with a guarantee that
/// `Drop` will tear it down. Sets `TMUX_TMPDIR`, `HOME`, and
/// `XDG_CONFIG_HOME` so any tmux/ADE code in the test process talks
/// to this server and sees an empty config dir.
pub(crate) struct IsolatedTmux {
    pub socket_dir: PathBuf,
    pub home: PathBuf,
    /// Saved values of the env vars we override, so Drop can restore.
    saved_env: Vec<(String, Option<String>)>,
}

impl IsolatedTmux {
    /// Start a fresh tmux server on a unique TMUX_TMPDIR + isolated
    /// HOME. Returns once the server is responding to commands.
    /// Panics on failure — tests that need tmux can't proceed.
    pub fn spawn() -> Self {
        let pid = std::process::id();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let stem = format!("ade-test-{}-{}", pid, n);
        let socket_dir = std::env::temp_dir().join(format!("{}-tmux", stem));
        let home = std::env::temp_dir().join(format!("{}-home", stem));
        std::fs::create_dir_all(&socket_dir).expect("mk socket dir");
        std::fs::create_dir_all(&home).expect("mk home dir");

        // Save the env we're about to mutate so Drop can restore it.
        let saved_env = ["TMUX_TMPDIR", "TMUX", "HOME", "XDG_CONFIG_HOME"]
            .iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect();

        // Set the new env. SAFETY: protected by ACCEPTANCE_LOCK held
        // by the caller.
        std::env::set_var("TMUX_TMPDIR", &socket_dir);
        std::env::remove_var("TMUX"); // we are not "inside" a tmux client
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));

        let me = IsolatedTmux {
            socket_dir,
            home,
            saved_env,
        };
        let _ = me.tmux(&["start-server"]).output();
        let ready = poll_until(Duration::from_secs(2), || {
            me.tmux(&["list-sessions"]).output().is_ok()
        });
        assert!(ready, "tmux server failed to start in {:?}", me.socket_dir);
        me
    }

    /// Build a `tmux` Command pointing at this server. `-f /dev/null`
    /// strips user config so tests are deterministic. We rely on
    /// TMUX_TMPDIR (set in `spawn`) to route to our isolated socket
    /// dir; we don't need an explicit `-L` because we're the only
    /// tmux server inside that dir.
    pub fn tmux(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("tmux");
        cmd.args(["-f", "/dev/null"]);
        for a in args {
            cmd.arg(a);
        }
        cmd
    }

    /// Create a new tmux session running `bash --norc --noprofile`
    /// (so prompts are deterministic and dotfiles can't influence
    /// the test). `cols`/`rows` set the initial pane dimensions —
    /// most tests just use 80x24.
    ///
    /// Status line is disabled so `pane_height` equals `client_height`
    /// — important for resize-forwarding assertions, which would
    /// otherwise see an off-by-one as tmux carves a row for status
    /// once a client attaches.
    pub fn new_session(&self, name: &str, cols: u16, rows: u16) -> Result<(), String> {
        let cols_s = cols.to_string();
        let rows_s = rows.to_string();
        let out = self
            .tmux(&[
                "new-session",
                "-d",
                "-s",
                name,
                "-x",
                &cols_s,
                "-y",
                &rows_s,
                "bash",
                "--norc",
                "--noprofile",
            ])
            .output()
            .map_err(|e| format!("tmux new-session: {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "tmux new-session failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        // Disable status line: server-wide setting so any subsequent
        // attach doesn't reserve a row. Don't ignore failure — a
        // silent skip here would re-introduce the resize off-by-one
        // and surface as a confusing test failure later (Codex
        // Phase-9 review).
        let status_out = self
            .tmux(&["set-option", "-g", "status", "off"])
            .output()
            .map_err(|e| format!("tmux set-option status off: {}", e))?;
        if !status_out.status.success() {
            return Err(format!(
                "tmux set-option status off failed: {}",
                String::from_utf8_lossy(&status_out.stderr).trim()
            ));
        }
        // Force a deterministic prompt so tests can match on `$ `.
        // Sent via send-keys so it lands inside the new session's shell.
        self.send_keys(name, "PS1='$ '\r")?;
        Ok(())
    }

    /// Run `tmux capture-pane -p -t '=name:'` and return the visible
    /// content. Note: text only (no -e), since acceptance tests check
    /// for substrings, not styles.
    pub fn capture(&self, session: &str) -> Result<String, String> {
        let target = format!("={}:", session);
        let out = self
            .tmux(&["capture-pane", "-p", "-t", &target])
            .output()
            .map_err(|e| format!("capture-pane: {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "capture-pane failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Send literal keys to the session's active pane.
    pub fn send_keys(&self, session: &str, keys: &str) -> Result<(), String> {
        let target = format!("={}:", session);
        let out = self
            .tmux(&["send-keys", "-t", &target, "-l", keys])
            .output()
            .map_err(|e| format!("send-keys: {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "send-keys failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Query tmux for the active pane's reported dimensions. Used by
    /// resize-forwarding assertions.
    pub fn pane_size(&self, session: &str) -> Result<(u16, u16), String> {
        let target = format!("={}:", session);
        let out = self
            .tmux(&[
                "display-message",
                "-p",
                "-t",
                &target,
                "#{pane_width};#{pane_height}",
            ])
            .output()
            .map_err(|e| format!("display-message: {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "display-message failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let s = String::from_utf8_lossy(&out.stdout);
        let mut parts = s.trim().split(';');
        let w: u16 = parts
            .next()
            .ok_or("missing width")?
            .parse()
            .map_err(|_| "bad width")?;
        let h: u16 = parts
            .next()
            .ok_or("missing height")?
            .parse()
            .map_err(|_| "bad height")?;
        Ok((w, h))
    }

    /// `true` when the named session exists on this server.
    pub fn has_session(&self, session: &str) -> bool {
        let target = format!("={}:", session);
        self.tmux(&["has-session", "-t", &target])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// `true` when the active pane of `session` is in some mode
    /// (copy-mode, view-mode, etc.). Used by mouse-passthrough
    /// acceptance tests to verify scroll-up entered copy-mode.
    pub fn pane_in_mode(&self, session: &str) -> Result<bool, String> {
        let target = format!("={}:", session);
        let out = self
            .tmux(&[
                "display-message",
                "-p",
                "-t",
                &target,
                "#{pane_in_mode}",
            ])
            .output()
            .map_err(|e| format!("display-message: {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "pane_in_mode failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim() == "1")
    }

    /// Set a tmux server-global option. Used by tests that need
    /// non-default behaviour (e.g. `mouse on` for the mouse-passthrough
    /// acceptance test).
    pub fn set_option(&self, key: &str, value: &str) -> Result<(), String> {
        let out = self
            .tmux(&["set-option", "-g", key, value])
            .output()
            .map_err(|e| format!("set-option {}: {}", key, e))?;
        if !out.status.success() {
            return Err(format!(
                "set-option {} {} failed: {}",
                key,
                value,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Kill the named session. Used by edge-case tests that simulate
    /// the target session disappearing while the user is embedded.
    pub fn kill_session(&self, session: &str) -> Result<(), String> {
        let target = format!("={}:", session);
        let out = self
            .tmux(&["kill-session", "-t", &target])
            .output()
            .map_err(|e| format!("kill-session: {}", e))?;
        if !out.status.success() {
            return Err(format!(
                "kill-session failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
}

impl Drop for IsolatedTmux {
    fn drop(&mut self) {
        // Best-effort kill — Drop must not panic. If the server's
        // already dead the kill-server will exit non-zero; we ignore.
        let _ = self.tmux(&["kill-server"]).output();
        // Restore env vars to whatever they were before spawn().
        for (k, v) in self.saved_env.drain(..) {
            match v {
                Some(val) => std::env::set_var(&k, val),
                None => std::env::remove_var(&k),
            }
        }
        // Best-effort cleanup of temp dirs — fine to leak on failure.
        let _ = std::fs::remove_dir_all(&self.socket_dir);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Wait until `f` returns true or `timeout` elapses. Polls every
/// 20ms — fine-grained enough for sub-second feedback without burning
/// CPU. Returns `true` if the predicate fired, `false` on timeout.
pub(crate) fn poll_until<F>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Convenience: poll until the given session's captured content
/// contains `needle`, or fail. Used heavily in the acceptance tests.
pub(crate) fn poll_for_capture_contains(
    tmux: &IsolatedTmux,
    session: &str,
    needle: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut last_capture = String::new();
    let ok = poll_until(timeout, || {
        match tmux.capture(session) {
            Ok(s) => {
                last_capture = s;
                last_capture.contains(needle)
            }
            Err(_) => false,
        }
    });
    if ok {
        Ok(last_capture)
    } else {
        Err(format!(
            "timed out waiting for {:?} in session {}; last capture:\n{}",
            needle, session, last_capture
        ))
    }
}

#[cfg(test)]
mod harness_self_tests {
    //! Sanity-check the harness itself before relying on it from
    //! Phase-9 acceptance tests. If `IsolatedTmux::new_session` is
    //! flaky, every downstream acceptance failure looks confusing.

    use super::*;

    #[cfg(unix)]
    #[test]
    fn isolated_tmux_session_lifecycle() {
        let _lock = acquire_acceptance_lock();
        let tmux = IsolatedTmux::spawn();
        assert!(!tmux.has_session("nope"));
        tmux.new_session("hello", 80, 24).expect("new-session");
        assert!(tmux.has_session("hello"));
        // Send a command that emits something deterministic.
        tmux.send_keys("hello", "echo harness-ok\r")
            .expect("send-keys");
        let cap = poll_for_capture_contains(
            &tmux,
            "hello",
            "harness-ok",
            Duration::from_secs(3),
        )
        .expect("capture should contain output");
        assert!(cap.contains("harness-ok"));
        // Pane size sanity.
        let (w, h) = tmux.pane_size("hello").expect("pane-size");
        assert_eq!((w, h), (80, 24));
        // Drop kills the server.
    }
}
