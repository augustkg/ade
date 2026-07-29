//! `ade peek <session> --json` — **local-only**. Surfaces what a Claude session
//! is asking (the pending permission prompt + options, from the hook-time
//! snapshot) plus a short recap from its transcript, for the HUD.
//!
//! Content stays on the host: this reads local cache files + the local
//! transcript and prints to stdout. It never touches remote hosts, and the
//! caller (the g1-hud server, running on the same host) owns any decision about
//! exposing the result. Everything surfaced is sanitised, bounded, and treated
//! as untrusted (see `prompt_snapshot`). The command fails closed — an
//! unparseable prompt reports `unavailable`, never a fabricated menu.

use std::process::Command;

use serde::Serialize;

use crate::prompt_snapshot::{self, Ask};

/// A short "what's been happening" summary drawn from the transcript.
#[derive(Debug, Serialize)]
struct Recap {
    /// `transcript` when lines were recovered, else `unavailable`.
    source: &'static str,
    lines: Vec<String>,
}

impl Recap {
    fn unavailable() -> Recap {
        Recap {
            source: "unavailable",
            lines: Vec::new(),
        }
    }
}

/// The `ade peek --json` payload. Additive/stable, same discipline as
/// `json_out`'s DTOs.
#[derive(Debug, Serialize)]
struct PeekOutput {
    machine: &'static str, // always "local" — peek is local-only
    session: String,
    session_id: Option<String>, // tmux #{session_id}
    pane: Option<String>,       // the resolved Claude pane_id
    state: &'static str,        // awaiting_approval | working | idle | none
    claude_session_id: Option<String>,
    ask: Ask,
    recap: Recap,
}

/// Small view of a per-pane status file — just the fields peek needs.
struct StatusLite {
    state: String,
    transcript_path: Option<String>,
    session_id: Option<String>,
}

pub fn run_peek(args: &[String]) -> ! {
    let (session_arg, json) = match parse_args(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(2);
        }
    };
    if !json {
        eprintln!("Error: this command requires --json");
        std::process::exit(2);
    }

    let session = match session_arg.or_else(current_session) {
        Some(s) => s,
        None => {
            eprintln!(
                "Error: no session given and not inside a tmux session — \
                 pass a session name (peek targets local sessions)"
            );
            std::process::exit(2);
        }
    };

    let panes = list_panes(&session);
    if panes.is_empty() {
        eprintln!("Error: no local tmux session named '{}'", session);
        std::process::exit(2);
    }

    // Pick the pane with the most attention-grabbing Claude state — mirrors the
    // Idle < Working < AwaitingApproval rollup the TUI uses.
    let session_id = panes.first().map(|(sid, _)| sid.clone());
    let mut chosen: Option<(String, StatusLite, u8)> = None;
    for (_, pane) in &panes {
        if let Some(st) = read_status(pane) {
            let rank = state_rank(&st.state);
            if chosen.as_ref().map_or(true, |(_, _, r)| rank > *r) {
                chosen = Some((pane.clone(), st, rank));
            }
        }
    }

    let (pane, state, transcript_path, claude_session_id) = match chosen {
        Some((pane, st, _)) => (
            Some(pane),
            normalize_state(&st.state),
            st.transcript_path,
            st.session_id,
        ),
        None => (None, "none", None, None),
    };

    // Only surface the live prompt when the session is actually awaiting one.
    let ask = match (state, &pane) {
        ("awaiting_approval", Some(p)) => prompt_snapshot::read(p),
        _ => Ask::unavailable(),
    };

    let recap = match transcript_path.as_deref() {
        Some(path) => recap_from_transcript(path),
        None => Recap::unavailable(),
    };

    let out = PeekOutput {
        machine: "local",
        session,
        session_id,
        pane,
        state,
        claude_session_id,
        ask,
        recap,
    };

    match serde_json::to_string_pretty(&out) {
        Ok(s) => {
            println!("{}", s);
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Error: serialize json: {}", e);
            std::process::exit(1);
        }
    }
}

/// Returns `(session, json_flag)`. The first non-flag argument is the session.
fn parse_args(args: &[String]) -> Result<(Option<String>, bool), String> {
    let mut session = None;
    let mut json = false;
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown argument '{}'", other));
            }
            other => {
                if session.is_some() {
                    return Err("expected a single session name".to_string());
                }
                session = Some(other.to_string());
            }
        }
    }
    Ok((session, json))
}

fn state_rank(state: &str) -> u8 {
    match state {
        "awaiting_approval" => 3,
        "working" => 2,
        "idle" => 1,
        _ => 0,
    }
}

fn normalize_state(state: &str) -> &'static str {
    match state {
        "awaiting_approval" => "awaiting_approval",
        "working" => "working",
        "idle" => "idle",
        _ => "none",
    }
}

/// The tmux session this process runs inside, if any (matches `ade kanban`).
fn current_session() -> Option<String> {
    if std::env::var_os("TMUX").is_none() {
        return None;
    }
    let out = Command::new("tmux")
        .args(["display-message", "-p", "#{session_name}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// `(session_id, pane_id)` for every pane in the named session, or empty on any
/// tmux error / no such session.
fn list_panes(session: &str) -> Vec<(String, String)> {
    let target = format!("={}", session);
    let out = match Command::new("tmux")
        .args([
            "list-panes",
            "-s",
            "-t",
            &target,
            "-F",
            "#{session_id}\t#{pane_id}",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            l.split_once('\t')
                .map(|(sid, pane)| (sid.to_string(), pane.to_string()))
        })
        .collect()
}

fn status_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    Some(home.join(".cache").join("ade").join("claude-status"))
}

fn read_status(pane: &str) -> Option<StatusLite> {
    let dir = status_dir()?;
    let body = std::fs::read_to_string(dir.join(format!("{}.json", pane))).ok()?;
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let state = v.get("state")?.as_str()?.to_string();
    Some(StatusLite {
        state,
        transcript_path: v
            .get("transcript_path")
            .and_then(|x| x.as_str())
            .map(String::from),
        session_id: v.get("session_id").and_then(|x| x.as_str()).map(String::from),
    })
}

/// Read the tail of the transcript and reconstruct a short recap: the last
/// assistant message's text, plus the recently-used tool names.
fn recap_from_transcript(path: &str) -> Recap {
    let lines = match tail_lines(path, 256 * 1024, 400) {
        Some(l) if !l.is_empty() => l,
        _ => return Recap::unavailable(),
    };

    let mut text_lines: Vec<String> = Vec::new();
    let mut recent_tools: Vec<String> = Vec::new();
    for raw in lines.iter().rev() {
        let v: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for block in content {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                        for l in prompt_snapshot::sanitize(t) {
                            text_lines.push(l);
                        }
                    }
                }
                Some("tool_use") => {
                    if let Some(n) = block.get("name").and_then(|x| x.as_str()) {
                        if recent_tools.len() < 6 {
                            recent_tools.push(n.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        // Stop at the most recent assistant message that actually said something.
        if !text_lines.is_empty() {
            break;
        }
    }

    let mut out: Vec<String> = text_lines.into_iter().take(8).collect();
    if !recent_tools.is_empty() {
        recent_tools.reverse(); // roughly chronological
        let joined = recent_tools.join(", ");
        let line = format!("· recent: {}", joined);
        out.push(line.chars().take(200).collect());
    }

    if out.is_empty() {
        Recap::unavailable()
    } else {
        Recap {
            source: "transcript",
            lines: out,
        }
    }
}

/// Read up to `max_bytes` from the end of a file and return its complete lines
/// (dropping a partial first line), at most `max_lines` from the tail. UTF-8 is
/// decoded lossily so a mid-character cut at the read boundary can't fail it.
fn tail_lines(path: &str, max_bytes: u64, max_lines: usize) -> Option<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    f.take(max_bytes).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // the first line is probably truncated
    }
    if lines.len() > max_lines {
        lines = lines.split_off(lines.len() - max_lines);
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_takes_session_and_json() {
        let a = vec!["mysess".to_string(), "--json".to_string()];
        let (s, j) = parse_args(&a).unwrap();
        assert_eq!(s.as_deref(), Some("mysess"));
        assert!(j);
    }

    #[test]
    fn parse_args_rejects_two_sessions_and_unknown_flags() {
        assert!(parse_args(&["a".to_string(), "b".to_string()]).is_err());
        assert!(parse_args(&["--nope".to_string()]).is_err());
    }

    #[test]
    fn state_rank_orders_awaiting_highest() {
        assert!(state_rank("awaiting_approval") > state_rank("working"));
        assert!(state_rank("working") > state_rank("idle"));
        assert!(state_rank("idle") > state_rank("bogus"));
    }

    #[test]
    fn recap_extracts_last_assistant_text_and_tools() {
        // Two assistant turns; the recap should surface the LAST text turn.
        let dir = std::env::temp_dir().join(format!("ade-peek-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let body = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Closing the workstream; work merged in PR #67."},{"type":"tool_use","name":"Edit"}]}}"#,
            "\n"
        );
        std::fs::write(&path, body).unwrap();
        let r = recap_from_transcript(path.to_str().unwrap());
        assert_eq!(r.source, "transcript");
        assert!(r.lines.iter().any(|l| l.contains("merged in PR #67")), "{:?}", r.lines);
        assert!(r.lines.iter().any(|l| l.contains("Edit")), "{:?}", r.lines);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recap_unavailable_for_missing_transcript() {
        assert_eq!(recap_from_transcript("/no/such/transcript.jsonl").source, "unavailable");
    }
}
