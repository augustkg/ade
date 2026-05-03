//! Suspend/resume mechanics around spawn-and-wait attaches.
//!
//! ADE's main loop spawns `tmux attach` (or `ssh` / `mosh` to a remote
//! tmux) as a child process and waits for it to exit, returning the user
//! to the TUI in the same tab. The child needs the terminal in a "normal"
//! state (cooked mode, no alternate screen) and ADE needs to restore TUI
//! state once the child returns.
//!
//! Signal handling: while the child runs, the kernel delivers tty signals
//! (Ctrl+C, Ctrl+Z, Ctrl+\) to the foreground process group. ADE is the
//! group leader and the child is a member, so without intervention both
//! receive the signal and ADE may die mid-attach. We install SIG_IGN for
//! SIGINT/SIGTSTP/SIGQUIT in ADE for the duration of the wait. The child
//! must restore SIG_DFL for those signals (plus SIGCHLD) via `pre_exec`
//! before its own exec — without that step, dispositions of SIG_IGN
//! inherited from ADE's address space would mean Ctrl+C in a remote shell
//! does nothing, which is exactly the regression we're avoiding.
//!
//! Foreground process-group transfer (`tcsetpgrp`) is the textbook
//! implementation but adds significant plumbing for marginal benefit over
//! the SIG_IGN approach. v1 ships with SIG_IGN; if Ctrl+C / Ctrl+Z ever
//! misbehave inside attached sessions, escalate to pgrp transfer.

use std::io::{self, Write};

use crossterm::{execute, terminal};
use ratatui::DefaultTerminal;

use crate::term_title;

/// Tear down the TUI's terminal state so a child process can take over
/// the tty. Call before `Command::spawn()`.
///
/// Signal-install ordering: SIG_IGN goes in *first*, before we leave
/// raw mode. Otherwise there's a window — between `disable_raw_mode`
/// (which restores ISIG / cooked mode) and signal-install — where the
/// kernel would translate a Ctrl+C keystroke into SIGINT delivered to
/// ADE's process group, killing the TUI before any child has been
/// spawned. Resume mirrors this: defaults are restored *last*.
pub fn suspend(terminal: &mut DefaultTerminal) -> io::Result<()> {
    install_signal_ignore();
    let _ = terminal.show_cursor();
    // Clear the OSC 0 title we've been emitting every frame so tmux's
    // `set-titles-string` (with `@ade-title`) takes over without
    // flashing ADE's row title. clear() also updates the term_title
    // cache so the post-resume `set` actually fires.
    term_title::clear();
    let mut out = io::stdout();
    execute!(out, terminal::LeaveAlternateScreen)?;
    out.flush()?;
    terminal::disable_raw_mode()?;
    Ok(())
}

/// Restore the TUI's terminal state after the child has exited. Call
/// after `child.wait()`.
///
/// Deliberately does NOT call `ratatui::init()` — that installs a fresh
/// panic hook on every call, leaking + chaining hooks across attach
/// cycles. We reuse the existing `DefaultTerminal` and re-establish raw
/// mode + alternate screen manually.
pub fn resume(terminal: &mut DefaultTerminal) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, terminal::EnterAlternateScreen)?;
    out.flush()?;
    // Force a full redraw on the next frame: ratatui's diff renderer
    // would otherwise think nothing changed since suspend (its internal
    // buffers don't know the screen got clobbered by the child program).
    terminal.clear()?;
    let _ = terminal.hide_cursor();
    // Restore default signal dispositions last — matches the suspend
    // ordering so we're never in cooked mode without SIG_IGN installed.
    restore_default_signals();
    Ok(())
}

#[cfg(unix)]
fn install_signal_ignore() {
    // SAFETY: setting signal dispositions to SIG_IGN is async-signal-safe
    // and idempotent; no shared state at risk.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
}

#[cfg(unix)]
fn restore_default_signals() {
    // SAFETY: as above — restoring SIG_DFL is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTSTP, libc::SIG_DFL);
        libc::signal(libc::SIGQUIT, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn install_signal_ignore() {}

#[cfg(not(unix))]
fn restore_default_signals() {}

/// Restore default signal dispositions in a freshly forked child before
/// it execs the attach command. Pass to `Command::pre_exec`. Without
/// this, the child inherits ADE's SIG_IGN dispositions and Ctrl+C /
/// Ctrl+Z stop working in remote shells.
#[cfg(unix)]
pub fn child_restore_default_signals() -> io::Result<()> {
    // SAFETY: pre_exec runs after fork in the child, before exec; the
    // child has a single thread and only async-signal-safe operations
    // are permitted. `signal(2)` qualifies.
    unsafe {
        if libc::signal(libc::SIGINT, libc::SIG_DFL) == libc::SIG_ERR
            || libc::signal(libc::SIGTSTP, libc::SIG_DFL) == libc::SIG_ERR
            || libc::signal(libc::SIGQUIT, libc::SIG_DFL) == libc::SIG_ERR
            || libc::signal(libc::SIGCHLD, libc::SIG_DFL) == libc::SIG_ERR
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
