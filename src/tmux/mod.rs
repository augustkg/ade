pub mod local;
pub mod remote;

use crate::claude_status::ClaudeState;
use crate::hosts::Host;
use crate::model::Machine;

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub name: String,
    pub windows: u32,
    pub attached: bool,
    /// `Some(state)` when at least one pane in this session has `claude` as
    /// its foreground process. Populated by the backend after merging
    /// `list-sessions`, `list-panes`, and the per-pane status files.
    pub claude: Option<ClaudeState>,
}

pub trait TmuxBackend {
    fn list_sessions(&self) -> Result<Vec<Session>, String>;
    fn create_session(&self, name: &str) -> Result<(), String>;
    fn rename_session(&self, old: &str, new: &str) -> Result<(), String>;
    fn kill_session(&self, name: &str) -> Result<(), String>;
}

/// True if ADE is being launched from inside a tmux pane. Checks `TMUX`
/// first (the canonical signal) and falls back to `TMUX_PANE`, which some
/// shell setups expose even when `TMUX` has been stripped.
pub fn is_inside_tmux() -> bool {
    std::env::var("TMUX").is_ok() || std::env::var("TMUX_PANE").is_ok()
}

/// When ADE is launched from inside tmux, returns the name of the session
/// the calling pane belongs to. Uses `#{session_name}` — `#{client_session}`
/// is empty when tmux is invoked as a subprocess (no client context), so
/// using it gave us false negatives on the same-session check.
pub fn current_session() -> Option<String> {
    if !is_inside_tmux() {
        return None;
    }
    let out = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{session_name}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub fn local() -> local::LocalTmux {
    local::LocalTmux
}

pub fn backend_for(machine: &Machine, hosts: &[Host]) -> Option<Box<dyn TmuxBackend>> {
    match machine {
        Machine::Local => Some(Box::new(local::LocalTmux)),
        Machine::Remote(name) => hosts.iter().find(|h| h.name == *name).map(|h| {
            Box::new(remote::RemoteTmux { host: h.clone() }) as Box<dyn TmuxBackend>
        }),
    }
}

pub(crate) fn parse_session_line(line: &str) -> Option<Session> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() >= 3 {
        Some(Session {
            name: parts[0].to_string(),
            windows: parts[1].parse().unwrap_or(0),
            attached: parts[2] == "1",
            claude: None,
        })
    } else {
        None
    }
}

/// Parse a single line of `tmux list-panes -a -F '#{session_name}\t#{pane_current_command}\t#{pane_id}\t#{pane_pid}'`
/// into `(session_name, pane_current_command, pane_id, pane_pid)`.
pub(crate) fn parse_pane_line(line: &str) -> Option<(String, String, String, u32)> {
    let parts: Vec<&str> = line.splitn(4, '\t').collect();
    if parts.len() != 4 {
        return None;
    }
    let session = parts[0];
    let cmd = parts[1];
    let pane_id = parts[2];
    let pane_pid: u32 = parts[3].parse().ok()?;
    if session.is_empty() || pane_id.is_empty() {
        return None;
    }
    Some((
        session.to_string(),
        cmd.to_string(),
        pane_id.to_string(),
        pane_pid,
    ))
}

/// Join `list-panes -a` output with the per-pane status map and return a map
/// of `session_name → ClaudeState`. A session is included in the result only
/// when at least one of its panes is **actively working** — i.e. has a
/// `claude` process in its descendant tree AND a `state=working` status
/// file. Idle panes (Claude loaded but waiting at the prompt) and panes
/// with no status file at all are intentionally skipped, so the `claude`
/// chip in the UI lights up only while a turn is in progress.
///
/// A pane is considered to be running Claude if either `pane_current_command`
/// is `claude` OR `pane_pid` is in the descendant set built from a `ps`
/// walk (catches shell-wrapped or background-launched Claude processes).
pub(crate) fn map_claude_states(
    panes_text: &str,
    statuses: &std::collections::HashMap<String, ClaudeState>,
    claude_pane_pids: &std::collections::HashSet<u32>,
) -> std::collections::HashMap<String, ClaudeState> {
    let mut out: std::collections::HashMap<String, ClaudeState> = std::collections::HashMap::new();
    for line in panes_text.lines() {
        let Some((session, cmd, pane_id, pane_pid)) = parse_pane_line(line) else {
            continue;
        };
        let is_claude = cmd == "claude" || claude_pane_pids.contains(&pane_pid);
        if !is_claude {
            continue;
        }
        // Only surface Working. A missing status file (hooks not installed
        // yet, or hooks installed but no prompt submitted) is treated the
        // same as Idle: no working signal observed → no chip.
        let Some(state) = statuses.get(&pane_id).copied() else {
            continue;
        };
        if !matches!(state, ClaudeState::Working) {
            continue;
        }
        out.entry(session)
            .and_modify(|cur| {
                if state > *cur {
                    *cur = state;
                }
            })
            .or_insert(state);
    }
    out
}
