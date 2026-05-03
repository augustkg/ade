pub mod local;
pub mod remote;

use crate::claude_status::{ClaudeState, Provenance};
use crate::hosts::Host;
use crate::model::Machine;

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub name: String,
    /// tmux's `#{session_id}` (e.g. `$3`). Stable across `rename-session`
    /// — used as the diff key for notification dispatch in
    /// `App::apply_refresh_result` so that mid-turn renames don't miss a
    /// "Claude finished" transition and kill-then-recreate-with-same-name
    /// doesn't fire a false positive.
    pub session_id: String,
    pub windows: u32,
    pub attached: bool,
    /// `Some(state)` when at least one pane in this session has `claude` as
    /// its foreground process AND that pane's status file says it's
    /// `Working` or `AwaitingApproval`. Populated by the backend after
    /// merging `list-sessions`, `list-panes`, and the per-pane status files.
    pub claude: Option<ClaudeState>,
    /// `true` when at least one Claude pane in this session had its state
    /// synthesised by `read_local_statuses_with_working_ttl` (TTL kicked in
    /// on a stale active file). Used by `App::apply_refresh_result` to
    /// suppress notification rule 5: a `Some(Working) → None` transition
    /// caused purely by TTL is not "Claude finished a turn".
    pub claude_demoted: bool,
}

/// Per-session rollup of every Claude pane in that session — produced by
/// `map_claude_states` and merged into `Session` fields by the backend.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ClaudeRollup {
    pub state: Option<ClaudeState>,
    pub demoted: bool,
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
    // Format must be:
    //   #{session_name}\t#{session_windows}\t#{session_attached}\t#{session_id}
    // The `session_id` column is required (no fallback) — without a stable
    // identity, the notification diff in `App::apply_refresh_result` can't
    // tell rename-mid-turn apart from kill-and-recreate-same-name.
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() >= 4 {
        Some(Session {
            name: parts[0].to_string(),
            windows: parts[1].parse().unwrap_or(0),
            attached: parts[2] == "1",
            session_id: parts[3].to_string(),
            claude: None,
            claude_demoted: false,
        })
    } else {
        None
    }
}

/// Parse a single line of `tmux list-panes -a -F '#{session_name}\t#{pane_current_command}\t#{pane_id}\t#{pane_pid}\t#{session_id}'`
/// into `(session_name, pane_current_command, pane_id, pane_pid, session_id)`.
///
/// The `session_id` column is what notification dispatch keys on — it
/// survives `rename-session` between the list-sessions and list-panes
/// calls, so a pane belongs to "the same tmux session as before" even
/// if `session_name` changed in flight. Older format strings without
/// `#{session_id}` parse with the trailing field missing — we fail
/// the parse rather than silently fall back, so the `5` here must be
/// matched by every caller's `-F` argument.
pub(crate) fn parse_pane_line(line: &str) -> Option<(String, String, String, u32, String)> {
    let parts: Vec<&str> = line.splitn(5, '\t').collect();
    if parts.len() != 5 {
        return None;
    }
    let session = parts[0];
    let cmd = parts[1];
    let pane_id = parts[2];
    let pane_pid: u32 = parts[3].parse().ok()?;
    let session_id = parts[4];
    if session.is_empty() || pane_id.is_empty() || session_id.is_empty() {
        return None;
    }
    Some((
        session.to_string(),
        cmd.to_string(),
        pane_id.to_string(),
        pane_pid,
        session_id.to_string(),
    ))
}

/// Join `list-panes -a` output with the per-pane status map and return a
/// per-session `ClaudeRollup`. A session ends up in the result if any of
/// its Claude panes had:
///   - an active state (`Working` or `AwaitingApproval`), recorded or not
///   - or a TTL-demoted reading (state synthesised to `Idle` from a stale
///     active file)
///
/// `rollup.state` is `Some(state)` only when an active pane's state was
/// observed — `Idle` panes (no chip rendered) and TTL-demoted panes
/// contribute nothing to it. `rollup.demoted` is `true` when any pane
/// in the session had `Provenance::Demoted` — the App's notification
/// diff uses this to suppress the false-positive "Claude finished"
/// banner that would otherwise fire on a TTL transition.
///
/// A pane is considered to be running Claude if either `pane_current_command`
/// is `claude` OR `pane_pid` is in the descendant set built from a `ps`
/// walk (catches shell-wrapped or background-launched Claude processes).
pub(crate) fn map_claude_states(
    panes_text: &str,
    statuses: &std::collections::HashMap<String, (ClaudeState, Provenance)>,
    claude_pane_pids: &std::collections::HashSet<u32>,
) -> std::collections::HashMap<String, ClaudeRollup> {
    let mut out: std::collections::HashMap<String, ClaudeRollup> =
        std::collections::HashMap::new();
    for line in panes_text.lines() {
        // We pull `session_id` out of pane lines too (see
        // `parse_pane_line`) but key the rollup on session NAME so the
        // existing list-sessions ↔ panes join (which uses name) keeps
        // working unchanged. The session_id field is present here as
        // future-proofing for a join-by-id refactor; not yet wired.
        let Some((session, cmd, pane_id, pane_pid, _session_id)) = parse_pane_line(line) else {
            continue;
        };
        let is_claude = cmd == "claude" || claude_pane_pids.contains(&pane_pid);
        if !is_claude {
            continue;
        }
        let Some(&(state, prov)) = statuses.get(&pane_id) else {
            continue;
        };
        let entry = out.entry(session).or_default();
        if matches!(
            state,
            ClaudeState::Working | ClaudeState::AwaitingApproval
        ) {
            entry.state = match entry.state {
                Some(cur) if state > cur => Some(state),
                None => Some(state),
                Some(cur) => Some(cur),
            };
        }
        if matches!(prov, Provenance::Demoted) {
            entry.demoted = true;
        }
    }
    out
}
