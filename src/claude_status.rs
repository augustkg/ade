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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClaudeState {
    /// Claude is loaded but waiting for user input. (Default when a pane
    /// runs `claude` but no status file exists — hooks may not be installed
    /// yet.)
    Idle,
    /// Claude is actively processing a turn (last hook event was
    /// `UserPromptSubmit`, no `Stop` since).
    Working,
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
pub fn parse_remote_statuses(text: &str) -> HashMap<String, ClaudeState> {
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
            out.insert(pane_id.trim().to_string(), state);
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
        _ => None,
    }
}

/// On refresh, any pane that no longer has Claude in its process tree but
/// still has a `state=working` status file on disk is "orphaned" — Claude
/// died (kill -9, crash, SSH drop, anything that fires no terminal hook)
/// without writing idle. Demote the file so a future Claude relaunched in
/// the same tmux pane doesn't inherit the stale working state.
///
/// Returns the count of files demoted. Caller MUST gate this on a
/// successful `ps` snapshot — if the descendant set is empty because `ps`
/// failed (not because Claude is actually gone), every pane would look
/// orphaned and we'd false-demote every working chip.
pub fn demote_orphan_working_files<I>(
    panes: I,
    claude_pane_pids: &HashSet<u32>,
    statuses: &HashMap<String, ClaudeState>,
) -> usize
where
    I: IntoIterator<Item = (String, String, u32)>,
{
    let dir = match status_dir() {
        Some(d) => d,
        None => return 0,
    };
    let mut demoted = 0;
    for (cmd, pane_id, pane_pid) in panes {
        let is_claude = cmd == "claude" || claude_pane_pids.contains(&pane_pid);
        if is_claude {
            continue;
        }
        if matches!(statuses.get(&pane_id), Some(ClaudeState::Working)) {
            let path = dir.join(format!("{}.json", pane_id));
            // Minimal idle payload — `at` is decorative (parser ignores it).
            if std::fs::write(&path, br#"{"state":"idle"}"#).is_ok() {
                demoted += 1;
            }
        }
    }
    demoted
}

fn status_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cache").join("ade").join("claude-status"))
}
