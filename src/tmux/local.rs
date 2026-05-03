use super::{map_claude_states, parse_pane_line, parse_session_line, Session, TmuxBackend};
use crate::claude_status;
use std::process::Command;

pub struct LocalTmux;

const LIST_FORMAT: &str =
    "#{session_name}\t#{session_windows}\t#{session_attached}\t#{session_id}";
const PANE_FORMAT: &str =
    "#{session_name}\t#{pane_current_command}\t#{pane_id}\t#{pane_pid}\t#{session_id}";

impl TmuxBackend for LocalTmux {
    fn list_sessions(&self) -> Result<Vec<Session>, String> {
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", LIST_FORMAT])
            .output();

        let mut sessions: Vec<Session> = match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                stdout.lines().filter_map(parse_session_line).collect()
            }
            // tmux exits non-zero when no server is running — treat as empty.
            Ok(_) => Vec::new(),
            Err(e) => return Err(format!("local tmux unavailable: {}", e)),
        };

        // Best-effort claude detection: failure is silent so the session
        // list still renders even if `list-panes` or `ps` misbehaves.
        let panes_text = Command::new("tmux")
            .args(["list-panes", "-a", "-F", PANE_FORMAT])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();

        // Track ps success explicitly: an empty ps_text would otherwise
        // make `find_claude_pane_pids` return an empty set, which makes
        // every pane look "orphaned" — and that would let the demotion
        // pass below false-demote every working chip.
        let (ps_text, ps_succeeded) = match Command::new("ps")
            .args(["-A", "-o", "pid,ppid,comm"])
            .output()
        {
            Ok(out) if out.status.success() => {
                (String::from_utf8_lossy(&out.stdout).into_owned(), true)
            }
            _ => (String::new(), false),
        };

        let pane_pids: Vec<u32> = panes_text
            .lines()
            .filter_map(parse_pane_line)
            .map(|(_, _, _, pid, _)| pid)
            .collect();
        let claude_pane_pids = claude_status::find_claude_pane_pids(&pane_pids, &ps_text);

        let statuses = claude_status::read_local_statuses_with_working_ttl();
        let claude_by_session = map_claude_states(&panes_text, &statuses, &claude_pane_pids);

        for s in &mut sessions {
            if let Some(rollup) = claude_by_session.get(&s.name) {
                s.claude = rollup.state;
                s.claude_demoted = rollup.demoted;
            }
        }

        // Catch panes whose Claude died without firing Stop / StopFailure /
        // SessionEnd (kill -9, crash, SSH drop). The hook chain can't help
        // here — only a process-aliveness check can. Skip if `ps` failed.
        if ps_succeeded {
            let panes_iter = panes_text
                .lines()
                .filter_map(parse_pane_line)
                .map(|(_, cmd, pid, ppid, _)| (cmd, pid, ppid));
            claude_status::demote_orphan_working_files(
                panes_iter,
                &claude_pane_pids,
                &statuses,
            );
        }

        Ok(sessions)
    }

    fn create_session(&self, name: &str) -> Result<(), String> {
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", name])
            .status()
            .map_err(|e| format!("Failed to create session: {}", e))?;

        if status.success() {
            Ok(())
        } else {
            Err("Failed to create tmux session".to_string())
        }
    }

    fn rename_session(&self, old: &str, new: &str) -> Result<(), String> {
        let target = format!("={}", old);
        let status = Command::new("tmux")
            .args(["rename-session", "-t", &target, new])
            .status()
            .map_err(|e| format!("Failed to rename session: {}", e))?;

        if status.success() {
            Ok(())
        } else {
            Err("Failed to rename tmux session".to_string())
        }
    }

    fn kill_session(&self, name: &str) -> Result<(), String> {
        let target = format!("={}", name);
        let status = Command::new("tmux")
            .args(["kill-session", "-t", &target])
            .status()
            .map_err(|e| format!("Failed to kill session: {}", e))?;

        if status.success() {
            Ok(())
        } else {
            Err("Failed to kill tmux session".to_string())
        }
    }
}

/// Capture the visible content of a session's active pane *with* ANSI
/// escape sequences so the renderer can preserve color and styling.
///
/// Target is `=name:` — the trailing colon is required: `capture-pane`
/// resolves a *pane* target, not a session target, and `=name` (without
/// the colon) trips the parser into "can't find pane" rather than
/// matching the session. The colon means "session=name exactly, default
/// window and pane," which is what we want. Errors on tmux spawn
/// failure or non-zero exit (e.g. session vanished).
pub fn capture_pane(name: &str) -> Result<String, String> {
    let target = format!("={}:", name);
    let out = Command::new("tmux")
        .args(["capture-pane", "-e", "-p", "-t", &target])
        .output()
        .map_err(|e| format!("tmux capture-pane failed to spawn: {}", e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("tmux capture-pane: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
