//! Reads per-pane Claude Code status files written by user-installed hooks.
//!
//! Each running Claude session that has the ADE hooks installed in
//! `~/.claude/settings.local.json` writes a small JSON file at
//! `~/.cache/ade/claude-status/<TMUX_PANE>.json` whenever the user submits a
//! prompt (state = working) or Claude finishes a turn (state = idle).
//!
//! ADE reads these files during refresh and joins by tmux `pane_id` so it can
//! tell whether a Claude pane is currently busy or sitting idle at the prompt.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Maximum age of a `state=working` cache file before we treat it as stale
/// and downgrade to `Idle` at read time. The hooks ADE installs include
/// `Stop`, `StopFailure`, `SessionEnd`, `Notification(idle_prompt)`, and
/// `SessionStart` — between them, every legitimate idle transition writes
/// the cache file fresh. A `working` file older than this constant means
/// every one of those hooks failed to fire (or hasn't been installed yet)
/// for a session that is no longer actively processing.
///
/// Set conservatively to absorb genuinely long turns (large compactions,
/// long-running tool calls). Tunable here if real users hit it. The TTL is
/// evaluated against the local file system mtime — local-only by design,
/// to side-step clock drift between hosts. Remote sessions rely on the
/// in-process Claude hooks (running with the remote's own clock) for
/// idle-state cleanup.
const WORKING_TTL: Duration = Duration::from_secs(4 * 60 * 60);

/// Per-session Claude state observed by ADE.
///
/// **Variant order is load-bearing.** `PartialOrd` is derived in declaration
/// order, and the rollup in `crate::tmux::map_claude_states` does
/// `if state > *cur { *cur = state }` — so the ordering picks which state
/// "wins" when a session has multiple Claude panes in different states.
/// `Idle < Working < AwaitingApproval` means an awaiting-approval pane
/// always shows the most attention-grabbing chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClaudeState {
    /// Claude is loaded but waiting for user input. (Default when a pane
    /// runs `claude` but no status file exists — hooks may not be installed
    /// yet.)
    Idle,
    /// Claude is actively processing a turn (last hook event was
    /// `UserPromptSubmit`, no `Stop` since).
    Working,
    /// Claude is waiting on the user to approve a permission prompt.
    /// Set by the `Notification[matcher=permission_prompt]` hook.
    AwaitingApproval,
}

/// How a `ClaudeState` reading reached us. Drives notification-suppression
/// rule 5 (TTL/orphan demotions are not "Claude finished a turn"). When
/// `read_local_statuses_with_working_ttl` or `demote_orphan_working_files`
/// synthesises an `Idle` from a stale `Working` cache file, the resulting
/// reading is `Demoted` even though it looks identical to a freshly-written
/// `Idle`. App-side diff logic skips notifications for `Demoted` transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// The state was read from a status file the hook actually wrote.
    Recorded,
    /// ADE synthesised this state — typically demoting a stale `Working`
    /// or `AwaitingApproval` because the file's mtime exceeded `WORKING_TTL`
    /// or because the Claude pane's pid is no longer in the descendant set.
    Demoted,
}

/// Read every `*.json` file under `~/.cache/ade/claude-status/` and return a
/// map of `pane_id` (e.g. `%37`) → state. Failure is silent — a missing or
/// unreadable directory just yields an empty map.
pub fn read_local_statuses() -> HashMap<String, ClaudeState> {
    let dir = match status_dir() {
        Some(d) => d,
        None => return HashMap::new(),
    };
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return HashMap::new(),
    };

    let mut out = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let pane_id = stem.to_string();
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(state) = parse_status_body(&body) {
            out.insert(pane_id, state);
        }
    }
    out
}

/// Same as `read_local_statuses` but applies a TTL to `Working` /
/// `AwaitingApproval` entries and tags each reading with its `Provenance`.
///
/// Reasoning: every legitimate "Claude is now idle" transition triggers one
/// of our hooks (`Stop`, `StopFailure`, `SessionEnd`,
/// `Notification(idle_prompt)`, `SessionStart`) which overwrites the cache
/// file with `state=idle`. A `working` (or `awaiting_approval`) cache file
/// that hasn't been touched in `WORKING_TTL` means *every* idle hook failed
/// to fire — the most common cause being that the hooks weren't installed
/// yet when the in-progress Claude session started, so it can't emit them
/// retroactively. In that case we'd rather show "no chip" than a stale chip
/// until the user submits another prompt.
///
/// Read-only — never modifies the cache file. The next legitimate hook
/// firing (or `SessionStart` on Claude relaunch) will overwrite naturally.
/// Local-only by design: `fs::metadata().modified()` is the local kernel's
/// view of the local filesystem, with no clock-skew exposure that would
/// false-demote on a host with a drifting clock.
///
/// `Provenance::Recorded` means the state came from a hook-written file
/// (or no demotion was needed). `Provenance::Demoted` means we synthesised
/// the `Idle` from a stale active file. The notification dispatch in
/// `App::apply_refresh_result` uses Provenance to suppress false-positive
/// "Claude finished" banners that would otherwise fire when TTL just ran.
pub fn read_local_statuses_with_working_ttl() -> HashMap<String, (ClaudeState, Provenance)> {
    let raw = read_local_statuses();
    let mut out: HashMap<String, (ClaudeState, Provenance)> = raw
        .into_iter()
        .map(|(k, v)| (k, (v, Provenance::Recorded)))
        .collect();
    let Some(dir) = status_dir() else {
        return out;
    };
    let now = SystemTime::now();
    for (pane_id, entry) in out.iter_mut() {
        // Only TTL the active states; Idle is already the "nothing to do"
        // state and demoting it again would be a no-op.
        if !matches!(
            entry.0,
            ClaudeState::Working | ClaudeState::AwaitingApproval
        ) {
            continue;
        }
        let path = dir.join(format!("{}.json", pane_id));
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let elapsed = now.duration_since(mtime).unwrap_or(Duration::ZERO);
        if elapsed > WORKING_TTL {
            *entry = (ClaudeState::Idle, Provenance::Demoted);
        }
    }
    out
}

/// Parse the concatenated remote-status payload produced by the SSH probe.
/// The wire format is the literal output of:
///
/// ```sh
/// for f in ~/.cache/ade/claude-status/*.json; do
///   [ -f "$f" ] || continue
///   printf '%s\n' "$(basename "$f" .json)"
///   cat "$f"
///   printf '\n---ADE-STATUS-END---\n'
/// done
/// ```
///
/// i.e. `<pane_id>\n<json body>\n---ADE-STATUS-END---\n`, repeated.
///
/// All entries are `Provenance::Recorded` — the remote probe does not
/// apply a TTL or orphan-walk (the remote tmux runs its own hook chain
/// against its own clock, so demotion happens server-side and we trust
/// what we read).
pub fn parse_remote_statuses(text: &str) -> HashMap<String, (ClaudeState, Provenance)> {
    let mut out = HashMap::new();
    for chunk in text.split("---ADE-STATUS-END---") {
        let trimmed = chunk.trim_start_matches('\n');
        if trimmed.trim().is_empty() {
            continue;
        }
        let mut lines = trimmed.lines();
        let Some(pane_id) = lines.next() else { continue };
        let body: String = lines.collect::<Vec<_>>().join("\n");
        if let Some(state) = parse_status_body(&body) {
            out.insert(pane_id.trim().to_string(), (state, Provenance::Recorded));
        }
    }
    out
}

/// Given the output of `ps -A -o pid,ppid,comm` and a list of pane root pids,
/// return the subset of those root pids whose process subtree contains any
/// process named `claude`.
///
/// Catches Claude even when it's not the pane's foreground process — e.g. a
/// shell wrapper is the immediate child and Claude is a grandchild, or the
/// user has temporarily backgrounded Claude under a build/REPL.
pub fn find_claude_pane_pids(pane_pids: &[u32], ps_text: &str) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut comm_by_pid: HashMap<u32, String> = HashMap::new();

    for line in ps_text.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let comm = parts.next().unwrap_or("");
        comm_by_pid.insert(pid, comm.to_string());
        children.entry(ppid).or_default().push(pid);
    }

    let mut out = HashSet::new();
    for &root in pane_pids {
        let mut stack = vec![root];
        // Per-root visited set guards against pathological `ps_text` (esp.
        // from a remote we don't fully trust) that could otherwise loop.
        let mut visited: HashSet<u32> = HashSet::new();
        while let Some(cur) = stack.pop() {
            if !visited.insert(cur) {
                continue;
            }
            if comm_by_pid.get(&cur).map(|c| c == "claude").unwrap_or(false) {
                out.insert(root);
                break;
            }
            if let Some(kids) = children.get(&cur) {
                stack.extend_from_slice(kids);
            }
        }
    }
    out
}

fn parse_status_body(body: &str) -> Option<ClaudeState> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let state = value.get("state")?.as_str()?;
    match state {
        "working" => Some(ClaudeState::Working),
        "idle" => Some(ClaudeState::Idle),
        "awaiting_approval" => Some(ClaudeState::AwaitingApproval),
        _ => None,
    }
}

/// On refresh, any pane that no longer has Claude in its process tree but
/// still has an active (`working` or `awaiting_approval`) status file on
/// disk is "orphaned" — Claude died (kill -9, crash, SSH drop, anything
/// that fires no terminal hook) without writing idle. Demote the file so a
/// future Claude relaunched in the same tmux pane doesn't inherit the
/// stale active state.
///
/// Returns the set of pane_ids that were demoted in this pass — fed back
/// into the in-memory rollup so the same refresh tick treats them as
/// `Provenance::Demoted` (suppressing notification-dispatch rule 5)
/// without waiting for the next tick to re-read the file.
///
/// Caller MUST gate this on a successful `ps` snapshot — if the descendant
/// set is empty because `ps` failed (not because Claude is actually gone),
/// every pane would look orphaned and we'd false-demote every active chip.
pub fn demote_orphan_working_files<I>(
    panes: I,
    claude_pane_pids: &HashSet<u32>,
    statuses: &HashMap<String, (ClaudeState, Provenance)>,
) -> HashSet<String>
where
    I: IntoIterator<Item = (String, String, u32)>,
{
    let mut demoted: HashSet<String> = HashSet::new();
    let dir = match status_dir() {
        Some(d) => d,
        None => return demoted,
    };
    for (cmd, pane_id, pane_pid) in panes {
        let is_claude = cmd == "claude" || claude_pane_pids.contains(&pane_pid);
        if is_claude {
            continue;
        }
        let active = matches!(
            statuses.get(&pane_id).map(|(s, _)| s),
            Some(ClaudeState::Working | ClaudeState::AwaitingApproval)
        );
        if !active {
            continue;
        }
        let path = dir.join(format!("{}.json", pane_id));
        // Minimal idle payload — `at` is decorative (parser ignores it).
        if std::fs::write(&path, br#"{"state":"idle"}"#).is_ok() {
            demoted.insert(pane_id);
        }
    }
    demoted
}

fn status_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cache").join("ade").join("claude-status"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_body_working() {
        let body = r#"{"state":"working","at":"2026-05-02T12:00:00Z"}"#;
        assert_eq!(parse_status_body(body), Some(ClaudeState::Working));
    }

    #[test]
    fn parse_status_body_idle() {
        let body = r#"{"state":"idle","at":"2026-05-02T12:00:00Z"}"#;
        assert_eq!(parse_status_body(body), Some(ClaudeState::Idle));
    }

    #[test]
    fn parse_status_body_unknown_state() {
        let body = r#"{"state":"thinking"}"#;
        assert_eq!(parse_status_body(body), None);
    }

    #[test]
    fn parse_status_body_garbage() {
        assert_eq!(parse_status_body("not json"), None);
        assert_eq!(parse_status_body(""), None);
        assert_eq!(parse_status_body("{}"), None);
    }

    #[test]
    fn parse_status_body_ignores_at_field() {
        // The parser is intentionally agnostic to `at` — TTL is enforced via
        // `read_local_statuses_with_working_ttl` using the file's mtime, not
        // the embedded timestamp. This test pins that invariant.
        let body = r#"{"state":"working","at":"not-a-real-timestamp"}"#;
        assert_eq!(parse_status_body(body), Some(ClaudeState::Working));
    }

    #[test]
    fn find_claude_pane_pids_descendant() {
        // Pane 100's child 200 is `claude` — pane 100 should be flagged.
        // Pane 300's tree has no claude — should not be flagged.
        let ps = "\
100 1 zsh\n\
200 100 claude\n\
300 1 zsh\n\
400 300 vim\n";
        let pids = vec![100, 300];
        let result = find_claude_pane_pids(&pids, ps);
        assert!(result.contains(&100));
        assert!(!result.contains(&300));
    }

    #[test]
    fn find_claude_pane_pids_handles_pid_cycle() {
        // Synthetic ps with a parent-child cycle: 100 -> 200 -> 100. The
        // per-root visited set must short-circuit instead of looping
        // forever. No claude in this tree — result must be empty.
        let ps = "\
100 200 zsh\n\
200 100 zsh\n";
        let pids = vec![100];
        let result = find_claude_pane_pids(&pids, ps);
        assert!(result.is_empty());
    }

    #[test]
    fn find_claude_pane_pids_pane_itself_is_claude() {
        // The pane root pid IS claude — should be flagged.
        let ps = "100 1 claude\n";
        let pids = vec![100];
        let result = find_claude_pane_pids(&pids, ps);
        assert!(result.contains(&100));
    }

    #[test]
    fn parse_remote_statuses_round_trip() {
        let text = "\
%5\n\
{\"state\":\"working\"}\n\
---ADE-STATUS-END---\n\
%17\n\
{\"state\":\"idle\"}\n\
---ADE-STATUS-END---\n\
%99\n\
{\"state\":\"awaiting_approval\"}\n\
---ADE-STATUS-END---\n";
        let result = parse_remote_statuses(text);
        assert_eq!(
            result.get("%5"),
            Some(&(ClaudeState::Working, Provenance::Recorded))
        );
        assert_eq!(
            result.get("%17"),
            Some(&(ClaudeState::Idle, Provenance::Recorded))
        );
        assert_eq!(
            result.get("%99"),
            Some(&(ClaudeState::AwaitingApproval, Provenance::Recorded))
        );
    }

    #[test]
    fn parse_status_body_awaiting_approval() {
        let body = r#"{"state":"awaiting_approval","at":"2026-05-02T12:00:00Z"}"#;
        assert_eq!(
            parse_status_body(body),
            Some(ClaudeState::AwaitingApproval)
        );
    }

    #[test]
    fn claude_state_partial_ord_idle_lt_working_lt_awaiting() {
        // Load-bearing: the rollup `if state > *cur { *cur = state }` in
        // tmux::map_claude_states relies on this ordering to prefer the
        // most attention-grabbing state when a session has multiple panes.
        assert!(ClaudeState::Idle < ClaudeState::Working);
        assert!(ClaudeState::Working < ClaudeState::AwaitingApproval);
        assert!(ClaudeState::Idle < ClaudeState::AwaitingApproval);
    }
}
