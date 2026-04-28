//! `ade install-hooks` implementation: idempotently merges ADE's
//! UserPromptSubmit + Stop hook entries into `~/.claude/settings.json`,
//! either locally or on a configured remote host via SSH. Also cleans up
//! stale entries from the legacy `~/.claude/settings.local.json` path,
//! which Claude Code does not actually load (only `~/.claude/settings.json`,
//! `.claude/settings.json`, and `.claude/settings.local.json` are loaded —
//! verified against Claude binary 2.1.119 and the official hooks docs).

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::hosts::{Config, Host};
use crate::ssh_io;

/// Marker substring embedded in our hook commands so we can recognise an
/// already-installed hook and avoid appending duplicates on repeat installs.
const MARKER: &str = "ade-status-marker";

const WORKING_CMD: &str = r#"true ade-status-marker; PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"; [ -z "$PANE" ] || (mkdir -p "$HOME/.cache/ade/claude-status" && printf '{"state":"working","at":"%s"}' "$(date -u +%FT%TZ)" > "$HOME/.cache/ade/claude-status/${PANE}.json")"#;

const IDLE_CMD: &str = r#"true ade-status-marker; PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"; [ -z "$PANE" ] || (mkdir -p "$HOME/.cache/ade/claude-status" && printf '{"state":"idle","at":"%s"}' "$(date -u +%FT%TZ)" > "$HOME/.cache/ade/claude-status/${PANE}.json")"#;

/// Check whether ADE's status hooks are already present in the local
/// `~/.claude/settings.json`. Returns `true` if our marker is found.
/// Failure to read is treated as "not installed".
pub fn is_installed_local() -> bool {
    let Some(path) = local_settings_path() else {
        return false;
    };
    let body = match fs::read_to_string(&path) {
        Ok(b) => b,
        Err(_) => return false,
    };
    body.contains(MARKER)
}

pub fn install_local() -> Result<String, String> {
    let path = local_settings_path().ok_or_else(|| "no $HOME set".to_string())?;
    let existing = read_settings_local(&path)?;
    let (updated, action) = merge_hooks(existing);
    let install_msg = if action.is_noop() {
        format!(
            "hooks already installed at {} — nothing to do",
            path.display()
        )
    } else {
        write_atomic(&path, &updated)?;
        format!(
            "installed ADE hooks at {} ({})",
            path.display(),
            action.summary()
        )
    };

    // Migrate any stragglers from the legacy ~/.claude/settings.local.json
    // path. Claude Code doesn't actually load that file, so any ADE marker
    // entries there are dead — remove them so they don't confuse future
    // diagnostics.
    let cleanup_msg = match cleanup_old_local_path() {
        Ok(0) => String::new(),
        Ok(n) => format!(
            "; also removed {} stale ADE entr{} from legacy ~/.claude/settings.local.json",
            n,
            if n == 1 { "y" } else { "ies" }
        ),
        Err(e) => format!("; warning: failed to clean legacy path: {}", e),
    };

    Ok(format!("{}{}", install_msg, cleanup_msg))
}

pub fn install_remote(config: &Config, host_name: &str) -> Result<String, String> {
    let host = config
        .host_by_name(host_name)
        .ok_or_else(|| format!("host '{}' not found in config", host_name))?;

    let existing_text = ssh_read_settings(host)?;
    let existing: Value = if existing_text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&existing_text)
            .map_err(|e| format!("parse remote settings.json: {}", e))?
    };

    let (updated, action) = merge_hooks(existing);
    if action.is_noop() {
        return Ok(format!(
            "{}: hooks already installed — nothing to do",
            host.name
        ));
    }

    let serialized = serde_json::to_string_pretty(&updated)
        .map_err(|e| format!("serialize: {}", e))?;
    ssh_write_settings(host, &serialized)?;

    Ok(format!(
        "{}: installed ADE hooks ({})",
        host.name,
        action.summary()
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventChange {
    Added,
    Updated,
    NoOp,
}

/// The set of Claude Code hook events ADE owns. Adding a new event is a
/// single-line change here; `merge_hooks` iterates this list and the
/// summary/no-op logic falls out automatically. `IDLE_CMD` is reused for
/// every "turn ended" variant — its body ignores stdin, so the differing
/// payloads of Stop vs StopFailure vs SessionEnd are harmless.
///
/// - `UserPromptSubmit` → working (user submitted a prompt)
/// - `Stop` → idle (turn finished normally)
/// - `StopFailure` → idle (turn failed with API error: rate_limit,
///   authentication_failed, server_error, etc. — `Stop` does not fire here)
/// - `SessionEnd` → idle (session exited normally: /clear, /resume, plain
///   exit. Does NOT cover `kill -9` — see process-aliveness follow-up)
const HOOK_EVENTS: &[(&str, &str)] = &[
    ("UserPromptSubmit", WORKING_CMD),
    ("Stop", IDLE_CMD),
    ("StopFailure", IDLE_CMD),
    ("SessionEnd", IDLE_CMD),
];

#[derive(Debug, Default)]
struct InstallAction {
    /// One entry per event in `HOOK_EVENTS`, recorded in iteration order.
    events: Vec<(&'static str, EventChange)>,
}

impl InstallAction {
    fn record(&mut self, event: &'static str, change: EventChange) {
        self.events.push((event, change));
    }
    fn is_noop(&self) -> bool {
        self.events
            .iter()
            .all(|(_, c)| matches!(c, EventChange::NoOp))
    }
    fn summary(&self) -> String {
        let added: Vec<&str> = self
            .events
            .iter()
            .filter(|(_, c)| matches!(c, EventChange::Added))
            .map(|(n, _)| *n)
            .collect();
        let updated: Vec<&str> = self
            .events
            .iter()
            .filter(|(_, c)| matches!(c, EventChange::Updated))
            .map(|(n, _)| *n)
            .collect();
        let mut parts: Vec<String> = Vec::new();
        if !added.is_empty() {
            parts.push(format!("added: {}", added.join(", ")));
        }
        if !updated.is_empty() {
            parts.push(format!("updated: {}", updated.join(", ")));
        }
        if parts.is_empty() {
            "no changes".to_string()
        } else {
            parts.join("; ")
        }
    }
}

fn merge_hooks(mut settings: Value) -> (Value, InstallAction) {
    if !settings.is_object() {
        settings = json!({});
    }
    let mut action = InstallAction::default();

    let hooks = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));

    if !hooks.is_object() {
        *hooks = json!({});
    }

    let hooks_obj = hooks.as_object_mut().unwrap();
    for (event_name, command) in HOOK_EVENTS {
        let change = ensure_event(hooks_obj, event_name, command);
        action.record(event_name, change);
    }

    (settings, action)
}

/// Ensure the given event's array contains the canonical ADE entry with the
/// expected `command` string. Returns `Added` if no marker entry existed,
/// `Updated` if a stale ADE entry was replaced (different command), or `NoOp`
/// if the existing ADE entry already matched. Non-ADE entries are never
/// touched.
fn ensure_event(
    hooks_obj: &mut serde_json::Map<String, Value>,
    event: &str,
    command: &str,
) -> EventChange {
    let arr = hooks_obj
        .entry(event.to_string())
        .or_insert_with(|| json!([]));
    if !arr.is_array() {
        *arr = json!([]);
    }
    let arr = arr.as_array_mut().unwrap();

    let canonical = json!({
        "hooks": [
            { "type": "command", "command": command }
        ]
    });

    match find_marker_entry(arr) {
        Some(idx) => {
            if arr[idx] == canonical {
                EventChange::NoOp
            } else {
                arr[idx] = canonical;
                EventChange::Updated
            }
        }
        None => {
            arr.push(canonical);
            EventChange::Added
        }
    }
}

/// Find the index of the first entry whose nested `hooks[].command` contains
/// our marker — i.e., the ADE-owned entry that we may need to update.
fn find_marker_entry(arr: &[Value]) -> Option<usize> {
    for (i, entry) in arr.iter().enumerate() {
        let Some(inner) = entry.get("hooks").and_then(|h| h.as_array()) else {
            continue;
        };
        for h in inner {
            if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                if cmd.contains(MARKER) {
                    return Some(i);
                }
            }
        }
    }
    None
}

fn local_settings_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".claude").join("settings.json"))
}

/// Legacy path ADE used to write to before we discovered Claude Code does
/// not load `~/.claude/settings.local.json`. Used only for cleanup —
/// `cleanup_old_local_path` removes our marker entries from this file so
/// users don't accumulate dead hook config in two places.
fn legacy_local_settings_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".claude").join("settings.local.json"))
}

/// Remove any ADE-marker-bearing entries from the legacy
/// `~/.claude/settings.local.json` file. Preserves all non-ADE entries
/// (other tools may have hooks there). Returns the number of entries
/// removed (0 if the file doesn't exist, has no ADE entries, or HOME isn't
/// set). Failure to write is surfaced as Err — callers may choose to log
/// rather than fail the whole install.
fn cleanup_old_local_path() -> Result<usize, String> {
    let Some(path) = legacy_local_settings_path() else {
        return Ok(0);
    };
    if !path.exists() {
        return Ok(0);
    }
    let body = fs::read_to_string(&path)
        .map_err(|e| format!("read legacy settings: {}", e))?;
    if body.trim().is_empty() {
        return Ok(0);
    }
    let mut value: Value = serde_json::from_str(&body)
        .map_err(|e| format!("parse legacy settings: {}", e))?;

    let removed = strip_ade_entries(&mut value);
    if removed == 0 {
        return Ok(0);
    }
    write_atomic(&path, &value)?;
    Ok(removed)
}

/// Remove every nested entry whose `command` contains MARKER from any
/// hook event array, regardless of event name. Iterates the entire `hooks`
/// object so we also clean up stragglers from future ADE versions if the
/// user downgrades. Returns the count of removed entries.
fn strip_ade_entries(value: &mut Value) -> usize {
    let Some(hooks) = value.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return 0;
    };
    let mut removed = 0;
    for (_event_name, arr_val) in hooks.iter_mut() {
        let Some(arr) = arr_val.as_array_mut() else {
            continue;
        };
        let before = arr.len();
        arr.retain(|entry| {
            let Some(inner) = entry.get("hooks").and_then(|h| h.as_array()) else {
                return true;
            };
            for h in inner {
                if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                    if cmd.contains(MARKER) {
                        return false;
                    }
                }
            }
            true
        });
        removed += before - arr.len();
    }
    removed
}

fn read_settings_local(path: &PathBuf) -> Result<Value, String> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let body = fs::read_to_string(path)
        .map_err(|e| format!("read {}: {}", path.display(), e))?;
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&body)
        .map_err(|e| format!("parse {}: {}", path.display(), e))
}

fn write_atomic(path: &PathBuf, value: &Value) -> Result<(), String> {
    let serialized = serde_json::to_string_pretty(value)
        .map_err(|e| format!("serialize: {}", e))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create dir {}: {}", parent.display(), e))?;
    }
    let tmp = path.with_extension("local.json.tmp");
    fs::write(&tmp, serialized)
        .map_err(|e| format!("write temp {}: {}", tmp.display(), e))?;
    fs::rename(&tmp, path)
        .map_err(|e| format!("rename into place: {}", e))?;
    Ok(())
}

fn ssh_read_settings(host: &Host) -> Result<String, String> {
    ssh_io::run(host, "cat ~/.claude/settings.json 2>/dev/null || true")
}

fn ssh_write_settings(host: &Host, body: &str) -> Result<(), String> {
    // Stream the new content over stdin to a remote shell that writes it via
    // a temp file and then atomically renames into place. No need for `jq`
    // or any remote tooling beyond a POSIX shell.
    let remote_cmd = "mkdir -p ~/.claude && cat > ~/.claude/settings.json.tmp && mv ~/.claude/settings.json.tmp ~/.claude/settings.json";
    ssh_io::run_with_stdin(host, remote_cmd, body.as_bytes()).map(|_| ())
}
