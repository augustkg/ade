//! `ade install-hooks` implementation: idempotently merges ADE's
//! UserPromptSubmit + Stop hook entries into `~/.claude/settings.local.json`,
//! either locally or on a configured remote host via SSH.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::hosts::{Config, Host};

/// Marker substring embedded in our hook commands so we can recognise an
/// already-installed hook and avoid appending duplicates on repeat installs.
const MARKER: &str = "ade-status-marker";

const WORKING_CMD: &str = r#"true ade-status-marker; [ -z "${TMUX_PANE:-}" ] || (mkdir -p "$HOME/.cache/ade/claude-status" && printf '{"state":"working","at":"%s"}' "$(date -u +%FT%TZ)" > "$HOME/.cache/ade/claude-status/${TMUX_PANE}.json")"#;

const IDLE_CMD: &str = r#"true ade-status-marker; [ -z "${TMUX_PANE:-}" ] || (mkdir -p "$HOME/.cache/ade/claude-status" && printf '{"state":"idle","at":"%s"}' "$(date -u +%FT%TZ)" > "$HOME/.cache/ade/claude-status/${TMUX_PANE}.json")"#;

/// Check whether ADE's status hooks are already present in the local
/// `~/.claude/settings.local.json`. Returns `true` if our marker is found.
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
    if action.is_noop() {
        return Ok(format!(
            "hooks already installed at {} — nothing to do",
            path.display()
        ));
    }
    write_atomic(&path, &updated)?;
    Ok(format!(
        "installed ADE hooks at {} ({})",
        path.display(),
        action.summary()
    ))
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
            .map_err(|e| format!("parse remote settings.local.json: {}", e))?
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

#[derive(Debug, Default)]
struct InstallAction {
    user_prompt_submit_added: bool,
    stop_added: bool,
}

impl InstallAction {
    fn is_noop(&self) -> bool {
        !self.user_prompt_submit_added && !self.stop_added
    }
    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.user_prompt_submit_added {
            parts.push("UserPromptSubmit");
        }
        if self.stop_added {
            parts.push("Stop");
        }
        format!("added: {}", parts.join(", "))
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

    action.user_prompt_submit_added =
        ensure_event(hooks.as_object_mut().unwrap(), "UserPromptSubmit", WORKING_CMD);
    action.stop_added =
        ensure_event(hooks.as_object_mut().unwrap(), "Stop", IDLE_CMD);

    (settings, action)
}

/// Append our hook entry to the given event's array if no existing entry
/// already carries our marker. Returns `true` if we appended, `false` if
/// nothing needed to change.
fn ensure_event(
    hooks_obj: &mut serde_json::Map<String, Value>,
    event: &str,
    command: &str,
) -> bool {
    let arr = hooks_obj
        .entry(event.to_string())
        .or_insert_with(|| json!([]));
    if !arr.is_array() {
        *arr = json!([]);
    }
    let arr = arr.as_array_mut().unwrap();

    if event_already_has_marker(arr) {
        return false;
    }

    arr.push(json!({
        "hooks": [
            { "type": "command", "command": command }
        ]
    }));
    true
}

fn event_already_has_marker(arr: &[Value]) -> bool {
    for entry in arr {
        let Some(inner) = entry.get("hooks").and_then(|h| h.as_array()) else {
            continue;
        };
        for h in inner {
            if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                if cmd.contains(MARKER) {
                    return true;
                }
            }
        }
    }
    false
}

fn local_settings_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".claude").join("settings.local.json"))
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

const SSH_OPTS: &[&str] = &[
    "-o",
    "ConnectTimeout=5",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
];

fn ssh_read_settings(host: &Host) -> Result<String, String> {
    let mut cmd = Command::new("ssh");
    cmd.args(SSH_OPTS);
    for a in &host.ssh_args {
        cmd.arg(a);
    }
    cmd.arg(&host.target);
    cmd.arg("cat ~/.claude/settings.local.json 2>/dev/null || true");
    let out = cmd.output().map_err(|e| format!("ssh failed: {}", e))?;
    if !out.status.success() && out.status.code() != Some(0) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("ssh read failed: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn ssh_write_settings(host: &Host, body: &str) -> Result<(), String> {
    // Stream the new content over stdin to a remote shell that writes it via
    // a temp file and then atomically renames into place. No need for `jq`
    // or any remote tooling beyond a POSIX shell.
    let remote_cmd = "mkdir -p ~/.claude && cat > ~/.claude/settings.local.json.tmp && mv ~/.claude/settings.local.json.tmp ~/.claude/settings.local.json";

    let mut cmd = Command::new("ssh");
    cmd.args(SSH_OPTS);
    for a in &host.ssh_args {
        cmd.arg(a);
    }
    cmd.arg(&host.target);
    cmd.arg(remote_cmd);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("ssh spawn: {}", e))?;
    {
        use std::io::Write;
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "no ssh stdin".to_string())?;
        stdin
            .write_all(body.as_bytes())
            .map_err(|e| format!("ssh write: {}", e))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("ssh wait: {}", e))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("ssh write failed: {}", stderr.trim()));
    }
    Ok(())
}
