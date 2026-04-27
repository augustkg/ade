//! Reads per-pane Claude Code status files written by user-installed hooks.
//!
//! Each running Claude session that has the ADE hooks installed in
//! `~/.claude/settings.local.json` writes a small JSON file at
//! `~/.cache/ade/claude-status/<TMUX_PANE>.json` whenever the user submits a
//! prompt (state = working) or Claude finishes a turn (state = idle).
//!
//! ADE reads these files during refresh and joins by tmux `pane_id` so it can
//! tell whether a Claude pane is currently busy or sitting idle at the prompt.

use std::collections::HashMap;
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

fn parse_status_body(body: &str) -> Option<ClaudeState> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let state = value.get("state")?.as_str()?;
    match state {
        "working" => Some(ClaudeState::Working),
        "idle" => Some(ClaudeState::Idle),
        _ => None,
    }
}

fn status_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cache").join("ade").join("claude-status"))
}
