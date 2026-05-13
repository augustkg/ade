pub mod local;
pub mod remote;

use crate::claude_status::{self, ClaudeState, Provenance, Reading};
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
    /// `true` when at least one pane in this session is running `claude`
    /// (or a descendant of `claude`) regardless of whether it's actively
    /// working. `claude` is `None` for idle Claude — we don't render a
    /// chip for it — but the duplicate flow still wants to fork an idle
    /// Claude into the new session, so it keys off this instead.
    pub claude_present: bool,
    /// Context-window percentage (0..=100) for this session's Claude pane,
    /// computed via `claude_status::context_window_pct` from the latest
    /// assistant turn the v3 hook script captured. `None` when the v3 hook
    /// hasn't fired yet (legacy v2 install, or Claude session started
    /// before its first assistant turn).
    pub claude_context_pct: Option<u8>,
}

/// Per-session rollup of every Claude pane in that session — produced by
/// `map_claude_states` and merged into `Session` fields by the backend.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ClaudeRollup {
    pub state: Option<ClaudeState>,
    pub demoted: bool,
    /// `true` as soon as we observe any Claude pane in the session,
    /// independent of whether a status file gave us a state. Idle Claude
    /// often has no status row (the hook only writes on state changes),
    /// so without this flag the rollup would say "no Claude here" for a
    /// session sitting at the prompt.
    pub present: bool,
    /// Highest context-window percentage observed across this session's
    /// Claude panes. `None` when no pane produced a `Reading` with usage
    /// data (legacy v2 hook install, or no assistant turn has happened
    /// yet). Used by the UI to render `claude · NN%`.
    pub context_pct: Option<u8>,
}

pub trait TmuxBackend {
    fn list_sessions(&self) -> Result<Vec<Session>, String>;
    fn create_session(&self, name: &str) -> Result<(), String>;
    fn rename_session(&self, old: &str, new: &str) -> Result<(), String>;
    fn kill_session(&self, name: &str) -> Result<(), String>;
    /// Create a new session in the same cwd as `source`. If `claude_running`,
    /// look up the source's Claude session-id (most recently modified jsonl
    /// in `~/.claude/projects/<encoded-cwd>/`) and launch
    /// `claude --resume <id> --fork-session` inside the new session — that
    /// branches the conversation cleanly instead of having two clients write
    /// to the same history. If the id can't be found, fall back to plain
    /// `claude`. If `claude_running` is false, just spawn the default shell
    /// in the new session at the source's cwd.
    fn duplicate_session(
        &self,
        source: &str,
        new_name: &str,
        claude_running: bool,
    ) -> Result<(), String>;
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

/// True if `s` looks like a Claude Code session UUID — the canonical
/// `8-4-4-4-12` hex layout. Mirrors the `case` glob used in the
/// `RemoteTmux::duplicate_session` shell script so local and remote
/// accept the exact same set of stems; without that symmetry, a
/// malformed `.jsonl` name would be passed to `claude --resume` locally
/// but fall back to plain `claude` remotely.
pub(crate) fn is_session_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        let is_dash_pos = i == 8 || i == 13 || i == 18 || i == 23;
        if is_dash_pos {
            if b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
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
            claude_present: false,
            claude_context_pct: None,
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
    statuses: &std::collections::HashMap<String, Reading>,
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
        // Mark presence as soon as we identify a Claude pane, BEFORE the
        // status-file lookup. Idle Claude often has no status row (the
        // hook only writes on state transitions), so requiring a status
        // to set `present` would miss exactly the case the duplicate
        // flow cares about most.
        let entry = out.entry(session).or_default();
        entry.present = true;
        let Some(reading) = statuses.get(&pane_id) else {
            continue;
        };
        if matches!(
            reading.state,
            ClaudeState::Working | ClaudeState::AwaitingApproval
        ) {
            entry.state = match entry.state {
                Some(cur) if reading.state > cur => Some(reading.state),
                None => Some(reading.state),
                Some(cur) => Some(cur),
            };
        }
        if matches!(reading.provenance, Provenance::Demoted) {
            entry.demoted = true;
        }
        // Surface the highest context % across all Claude panes in this
        // session. In practice there's only ever one (the duplicate-fork
        // flow makes new sessions, not multiple Claudes in the same one),
        // but the max-aggregation is the principled choice and costs
        // nothing.
        if let Some(usage) = reading.usage.as_ref() {
            let pct = claude_status::context_window_pct(usage);
            entry.context_pct = Some(match entry.context_pct {
                Some(cur) => cur.max(pct),
                None => pct,
            });
        }
    }
    out
}

#[cfg(test)]
mod claude_rollup_tests {
    //! Pins the detection semantics around `ClaudeRollup`:
    //!   * `present` flips on as soon as ANY Claude pane is observed,
    //!     even if the status file is missing — exactly the idle case
    //!     the Duplicate action needs to flag.
    //!   * `state` stays `None` for idle Claude (no status row) so the
    //!     refresh-time chip rendering is unchanged.
    //!   * The descendant-PID walk picks up shell-wrapped Claude too.

    use super::{map_claude_states, ClaudeRollup};
    use crate::claude_status::{ClaudeState, ContextUsage, Provenance, Reading};
    use std::collections::{HashMap, HashSet};

    fn pane_line(session: &str, cmd: &str, pane_id: &str, pid: u32, sid: &str) -> String {
        format!("{}\t{}\t{}\t{}\t{}", session, cmd, pane_id, pid, sid)
    }

    fn reading(state: ClaudeState, usage: Option<ContextUsage>) -> Reading {
        Reading {
            state,
            provenance: Provenance::Recorded,
            usage,
            seq: None,
        }
    }

    #[test]
    fn idle_claude_sets_present_even_without_status_file() {
        let panes =
            pane_line("sess", "claude", "%1", 1234, "$1") + "\n";
        let statuses: HashMap<String, Reading> = HashMap::new();
        let claude_pids: HashSet<u32> = HashSet::new();
        let out = map_claude_states(&panes, &statuses, &claude_pids);
        let rollup = out.get("sess").expect("rollup for idle Claude pane");
        assert!(
            rollup.present,
            "idle Claude with no status row must set present"
        );
        assert!(
            rollup.state.is_none(),
            "no chip for idle: state stays None"
        );
        assert!(!rollup.demoted);
        assert!(rollup.context_pct.is_none());
    }

    #[test]
    fn working_claude_sets_state_and_present() {
        let panes =
            pane_line("sess", "claude", "%1", 1234, "$1") + "\n";
        let mut statuses = HashMap::new();
        statuses.insert(
            "%1".to_string(),
            reading(ClaudeState::Working, None),
        );
        let claude_pids = HashSet::new();
        let out = map_claude_states(&panes, &statuses, &claude_pids);
        let rollup = out.get("sess").expect("rollup");
        assert!(rollup.present);
        assert_eq!(rollup.state, Some(ClaudeState::Working));
    }

    #[test]
    fn non_claude_pane_does_not_set_present() {
        let panes =
            pane_line("sess", "vim", "%1", 1234, "$1") + "\n";
        let statuses = HashMap::new();
        let claude_pids = HashSet::new();
        let out = map_claude_states(&panes, &statuses, &claude_pids);
        assert!(
            out.get("sess").is_none() || !out.get("sess").unwrap().present,
            "no Claude pane → rollup absent or present=false; got {:?}",
            out
        );
    }

    #[test]
    fn shell_wrapped_claude_via_descendant_pid_sets_present() {
        // pane_current_command is `bash`, but pane_pid is in the
        // descendant set — must still count as Claude.
        let panes =
            pane_line("sess", "bash", "%1", 5555, "$1") + "\n";
        let statuses = HashMap::new();
        let mut claude_pids = HashSet::new();
        claude_pids.insert(5555u32);
        let out = map_claude_states(&panes, &statuses, &claude_pids);
        let rollup: &ClaudeRollup = out
            .get("sess")
            .expect("descendant-pid Claude must be detected");
        assert!(rollup.present);
    }

    #[test]
    fn context_pct_surfaces_to_rollup() {
        let panes =
            pane_line("sess", "claude", "%1", 1234, "$1") + "\n";
        let mut statuses = HashMap::new();
        statuses.insert(
            "%1".to_string(),
            reading(
                ClaudeState::Idle,
                Some(ContextUsage {
                    tokens: 50_000,
                    model: "claude-opus-4-7".to_string(),
                    session_id: "s".to_string(),
                }),
            ),
        );
        let out = map_claude_states(&panes, &statuses, &HashSet::new());
        let rollup = out.get("sess").expect("rollup");
        assert_eq!(rollup.context_pct, Some(25), "50k of 200k = 25%");
    }
}
