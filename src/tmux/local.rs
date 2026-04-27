use super::{map_claude_states, parse_pane_line, parse_session_line, Session, TmuxBackend};
use crate::claude_status;
use std::process::Command;

pub struct LocalTmux;

const LIST_FORMAT: &str = "#{session_name}\t#{session_windows}\t#{session_attached}";
const PANE_FORMAT: &str =
    "#{session_name}\t#{pane_current_command}\t#{pane_id}\t#{pane_pid}";

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

        let ps_text = Command::new("ps")
            .args(["-A", "-o", "pid,ppid,comm"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();

        let pane_pids: Vec<u32> = panes_text
            .lines()
            .filter_map(parse_pane_line)
            .map(|(_, _, _, pid)| pid)
            .collect();
        let claude_pane_pids = claude_status::find_claude_pane_pids(&pane_pids, &ps_text);

        let statuses = claude_status::read_local_statuses();
        let claude_by_session = map_claude_states(&panes_text, &statuses, &claude_pane_pids);

        for s in &mut sessions {
            if let Some(state) = claude_by_session.get(&s.name) {
                s.claude = Some(*state);
            }
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
