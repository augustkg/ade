//! Inter-session mailbox: a file-based message bus that lets one Claude
//! Code session leave a message for another, routed by a running ADE.
//!
//! This mirrors the kanban intent inbox (`kanban.rs`): an external,
//! short-lived process (the `ade mail send` CLI, typically invoked by a
//! Claude Code session) never injects into another pane itself and never
//! writes `state.toml`. It only drops a *message file* into the inbox. The
//! long-lived App is the **sole router**: it drains the inbox each refresh
//! (~2s), surfaces pending mail in the TUI, and performs the one privileged
//! side effect — injecting the text into the recipient's pane — only when a
//! human triggers delivery (Track A). Automatic injection is a deliberately
//! separate, later, opt-in capability (Track B).
//!
//! Lifecycle is represented by which directory a message lives in, so every
//! transition is a single atomic `rename` (no in-place mutation, no torn
//! reads):
//!
//! ```text
//!   mail/inbox/<id>.json           pending  — visible, awaiting delivery
//!   mail/claimed/<id>.<pid>.json   claimed  — one router leased it to deliver
//!   mail/archive/<id>.json         injected attempt (audit trail; NOT proof
//!                                   Claude read or submitted it)
//!   mail/deadletter/<id>.json      expired / rejected / undeliverable
//!   *.bad                          quarantined malformed JSON
//! ```
//!
//! `send-keys -l` pastes the body *literally* into the recipient's prompt,
//! so the body is validated (no control chars, capped length) at both send
//! and deliver time — see `validate_body`. Validation is a safety guard, not
//! a trust boundary: any same-UID process can forge a file here, so the
//! router must verify recipient identity before injecting (that check lives
//! in the App, not this module).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Max bytes of a message body. `send-keys -l` pastes this straight into
/// the recipient's prompt; an oversized body is a DoS on the recipient TUI
/// and can exceed tmux's argument limits.
pub const MAX_BODY_BYTES: usize = 4 * 1024;

/// Cap on undelivered messages a single sender may have queued. Prevents one
/// runaway session from filling the inbox (a disk / starvation vector).
pub const MAX_PER_SENDER: usize = 64;

/// Cap on total undelivered messages across all senders.
pub const MAX_TOTAL: usize = 512;

/// Hard bound on how many inbox/claimed files a single scan will pull from
/// the directory. `MAX_TOTAL` is only enforced by cooperative writers, so a
/// hostile same-UID process could drop far more files; this caps the
/// per-refresh work (which runs on the UI thread) regardless. Set above
/// `MAX_TOTAL` so normal operation is never truncated.
pub const SCAN_CAP: usize = 1024;

/// Hard cap on the bytes read from any single message file. A forged inbox
/// file could be arbitrarily large; `read_inbox` runs on the UI thread every
/// refresh, so an unbounded read is a TUI-freeze vector. A valid message is
/// well under this (body ≤ 4 KiB plus small metadata).
pub const MAX_FILE_BYTES: u64 = 16 * 1024;

/// One message from one session to another. Pure payload — the lifecycle
/// state is the file's location (`inbox/` → `claimed/` → `archive/` or
/// `deadletter/`), not a field here, so every transition is a single atomic
/// `rename`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// `<millis>-<pid>-<seq>`, identical to the inbox filename stem. Doubles
    /// as the stable id across lifecycle moves and the ordering key.
    pub id: String,
    /// Sender's tmux session name.
    pub from_session: String,
    /// Sender's tmux `#{session_id}` (e.g. `$3`) captured at send time.
    /// Recorded for audit; session names are mutable and reused, so this is
    /// the more specific breadcrumb.
    pub from_session_id: String,
    /// Recipient's tmux session name.
    pub to_session: String,
    /// Machine the recipient lives on: `"local"`, or a host name from
    /// `hosts.toml`. Defaults to `"local"` so messages queued by an older
    /// build — and every message already sitting in an inbox — still parse.
    #[serde(default = "host_local")]
    pub to_host: String,
    /// Opaque address of the recipient session captured at send time:
    /// `"<tmux-server-pid>:<session_id>"` (e.g. `"94795:$5"`).
    ///
    /// The router re-resolves this live and refuses to inject unless it still
    /// matches, so mail can never land in a *different* conversation that
    /// happens to share the session name. The server pid is part of the address
    /// because `$N` session ids restart from `$0` when the tmux server
    /// restarts — session id alone would let a recreated session impersonate
    /// the original (Codex review).
    pub to_session_addr: String,
    /// The literal text to inject. Validated (see `validate_body`).
    pub body: String,
    /// Wall-clock at publish. Sender-controlled, so it is *not* trusted as
    /// the sole clock for TTL/expiry — the router uses the file's own
    /// first-seen/mtime for that.
    pub created_millis: u128,
}

impl Message {
    /// How the body is rendered into the recipient's prompt when delivered.
    /// The `[ADE mail …]` prefix tells the recipient's Claude the text is an
    /// inter-session message, not something its own user typed.
    pub fn render_injection(&self) -> String {
        format!("[ADE mail from {}] {}", self.from_session, self.body)
    }
}

/// The machine name meaning "where ADE itself runs". Matches the reserved host
/// name in `hosts::Config::upsert` and the `machine` field of `sessions --json`.
pub const HOST_LOCAL: &str = "local";

fn host_local() -> String {
    HOST_LOCAL.to_string()
}

/// Reject bodies that could do more than paste plain text into a prompt.
///
/// `send-keys -l` sends the body literally, so a bare `\n` would submit the
/// message mid-way and an ESC (`\x1b`) could trigger the recipient TUI's
/// keybindings. We therefore allow only non-control characters (this rejects
/// all C0/C1 controls incl. `\n`, `\r`, `\t`, ESC, and DEL) and cap length.
pub fn validate_body(body: &str) -> Result<(), String> {
    if body.is_empty() {
        return Err("message body is empty".to_string());
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(format!(
            "message body is {} bytes; the limit is {}",
            body.len(),
            MAX_BODY_BYTES
        ));
    }
    if let Some(bad) = body.chars().find(|c| is_forbidden_char(*c)) {
        return Err(format!(
            "message body contains a disallowed character (U+{:04X}); only \
             single-line printable text is allowed",
            bad as u32
        ));
    }
    Ok(())
}

/// Characters we refuse in a message body. Control chars (C0/C1 incl. `\n`,
/// `\r`, `\t`, ESC, DEL) would become key events under `send-keys -l`; the
/// Unicode bidi/format overrides can't inject keys but are pure
/// audit/UI-spoofing material, so we reject them too rather than render a
/// deceptive line into the recipient's prompt.
fn is_forbidden_char(c: char) -> bool {
    if c.is_control() {
        return true;
    }
    // Every Unicode format character (category Cf), not a hand-picked subset.
    //
    // The previous list covered the well-known bidi overrides and zero-width
    // marks but missed others in the same category — U+2060 WORD JOINER and
    // U+061C ARABIC LETTER MARK among them. Those cannot escape the shell or
    // tmux; what they do is render invisibly in the recipient's prompt and in
    // the audit trail, so a body can be made to display as something other than
    // what was sent. A partial list invites exactly that, so the rule is the
    // whole category. Raised by Codex review.
    matches!(c,
        '\u{00AD}'                      // SOFT HYPHEN
        | '\u{0600}'..='\u{0605}'       // Arabic number/format signs
        | '\u{061C}'                     // ARABIC LETTER MARK
        | '\u{06DD}' | '\u{070F}' | '\u{08E2}'
        | '\u{180E}'                     // MONGOLIAN VOWEL SEPARATOR
        | '\u{200B}'..='\u{200F}'       // zero-width + LRM/RLM
        | '\u{202A}'..='\u{202E}'       // bidi embeddings/overrides
        | '\u{2060}'..='\u{2064}'       // WORD JOINER + invisible operators
        | '\u{2066}'..='\u{206F}'       // isolates + deprecated formatting
        | '\u{FEFF}'                     // ZERO WIDTH NO-BREAK SPACE / BOM
        | '\u{FFF9}'..='\u{FFFB}'       // interlinear annotation
        | '\u{110BD}' | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}'     // Egyptian hieroglyph format controls
        | '\u{1BCA0}'..='\u{1BCA3}'     // shorthand format controls
        | '\u{1D173}'..='\u{1D17A}'     // musical beam/slur controls
        | '\u{E0001}' | '\u{E0020}'..='\u{E007F}' // language + TAG chars
    )
}

/// Final safety gate applied at the injection boundary (`TmuxBackend::
/// send_text`), so a forged inbox file that slipped past `read_inbox` — or
/// any future caller — still cannot push control or spoofing characters into
/// a live pane. Mirrors `validate_body` but allows a little extra length for
/// the rendered `[ADE mail …]` prefix.
pub fn validate_injection(text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("nothing to inject".to_string());
    }
    if text.len() > MAX_BODY_BYTES + 256 {
        return Err("injection text is too long".to_string());
    }
    if text.chars().any(is_forbidden_char) {
        return Err("injection text contains a disallowed character".to_string());
    }
    Ok(())
}

/// Whether a message id is safe to interpolate into a filesystem path. Our
/// generated ids are `<digits>-<pid>-<seq>`; restricting to `[0-9-]` rejects
/// any `/`, `.`, or `..` a forged inbox file might carry in its `id` field,
/// which would otherwise let a lifecycle `rename` escape the mail directory.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_digit() || b == b'-')
}

/// Validate a message read from disk: the payload's `id` must match the
/// filename it arrived under (so the audit id can't lie), the id must be
/// path-safe, the body must pass the sender-side checks, AND the *fully
/// rendered* injection (which also folds in `from_session`) must be safe to
/// type into a pane. Validating the rendered form here means a message that
/// survives `read_inbox` is guaranteed deliverable — a forged `from_session`
/// with control chars or excessive length is quarantined up front instead of
/// failing at the injection boundary and requeueing forever. Semantic-invalid
/// files are quarantined by `read_inbox`, not trusted.
fn message_is_sound(stem: &str, msg: &Message) -> bool {
    is_valid_id(stem)
        && msg.id == stem
        && validate_body(&msg.body).is_ok()
        && validate_injection(&msg.render_injection()).is_ok()
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `$XDG_CACHE_HOME/ade/mail`, else `~/.cache/ade/mail`. Per the XDG spec an
/// unset *or relative* `XDG_CACHE_HOME` must be ignored in favour of
/// `$HOME/.cache` — a relative value would otherwise root the mailbox at the
/// process's cwd.
pub fn mail_dir() -> Option<PathBuf> {
    let cache = match std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
        Some(p) if p.is_absolute() => p,
        _ => {
            let home = std::env::var_os("HOME").map(PathBuf::from)?;
            home.join(".cache")
        }
    };
    Some(cache.join("ade").join("mail"))
}

/// Create a directory (and parents) with owner-only permissions where the
/// platform supports it. Mail bodies can be sensitive; other local users
/// shouldn't be able to read or drop files into another user's mailbox.
fn create_dir_private(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {}", dir.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Publish a message into `<dir>/inbox` (see `write_message` for the default
/// dir). Factored to take an explicit base dir so it is hermetically
/// testable. Enforces body validation and the queue caps, then atomically
/// publishes via `create_new` on a `.tmp` + rename — the reader never sees a
/// partial write. Returns the message id (== inbox filename stem).
#[allow(clippy::too_many_arguments)]
pub fn write_message_in(
    dir: &Path,
    from_session: &str,
    from_session_id: &str,
    to_session: &str,
    to_host: &str,
    to_session_addr: &str,
    body: &str,
) -> Result<String, String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);

    validate_body(body)?;
    if from_session.is_empty() || to_session.is_empty() {
        return Err("sender and recipient session names must be non-empty".to_string());
    }
    if to_host.is_empty() {
        return Err("recipient host must be non-empty (use \"local\" for this machine)".to_string());
    }
    // The recipient's address is mandatory: it's how the router confirms it's
    // injecting into the same conversation the sender addressed (not a
    // same-name session created after a kill, or after a server restart).
    // Without it delivery can't be verified, so refuse to enqueue.
    if to_session_addr.is_empty() {
        return Err("could not resolve the recipient session's address".to_string());
    }
    if from_session == to_session {
        return Err("refusing to send a message to yourself".to_string());
    }

    let inbox = dir.join("inbox");
    create_dir_private(&inbox)?;

    // Queue caps: count pending messages total and from this sender. Reading
    // the inbox here is cheap at this volume and keeps the cap authoritative
    // even across processes.
    let pending = read_inbox_in(dir);
    if pending.len() >= MAX_TOTAL {
        return Err(format!(
            "mail inbox is full ({} pending, limit {}); delivery is backed up",
            pending.len(),
            MAX_TOTAL
        ));
    }
    let from_count = pending
        .iter()
        .filter(|(_, m)| m.from_session == from_session)
        .count();
    if from_count >= MAX_PER_SENDER {
        return Err(format!(
            "sender '{}' already has {} undelivered messages (limit {})",
            from_session, from_count, MAX_PER_SENDER
        ));
    }

    let millis = now_millis();
    for _ in 0..16 {
        let id = format!(
            "{:015}-{}-{:04}",
            millis,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let msg = Message {
            id: id.clone(),
            from_session: from_session.to_string(),
            from_session_id: from_session_id.to_string(),
            to_session: to_session.to_string(),
            to_host: to_host.to_string(),
            to_session_addr: to_session_addr.to_string(),
            body: body.to_string(),
            created_millis: millis,
        };
        let payload =
            serde_json::to_string(&msg).map_err(|e| format!("serialize message: {}", e))?;
        let tmp = inbox.join(format!("{}.tmp", id));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(mut f) => {
                use std::io::Write;
                f.write_all(payload.as_bytes())
                    .map_err(|e| format!("write message: {}", e))?;
                drop(f);
                let dest = inbox.join(format!("{}.json", id));
                fs::rename(&tmp, &dest).map_err(|e| format!("publish message: {}", e))?;
                return Ok(id);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create message file: {}", e)),
        }
    }
    Err("could not claim a unique message filename".to_string())
}

/// Publish a message into the default mailbox (`mail_dir()/inbox`).
pub fn write_message(
    from_session: &str,
    from_session_id: &str,
    to_session: &str,
    to_host: &str,
    to_session_addr: &str,
    body: &str,
) -> Result<String, String> {
    let dir = mail_dir().ok_or_else(|| "no $HOME / $XDG_CACHE_HOME set".to_string())?;
    write_message_in(
        &dir,
        from_session,
        from_session_id,
        to_session,
        to_host,
        to_session_addr,
        body,
    )
}

/// Read every pending message in `<dir>/inbox`, sorted by filename (=
/// publish order). Files are quarantined to `.bad` — never trusted — when
/// they are oversized, unparseable, or *semantically* invalid (id doesn't
/// match the filename, id isn't path-safe, or body fails validation), so a
/// forged or corrupt file can neither wedge the inbox nor slip a control
/// sequence through to delivery. `.tmp` files (mid-publish) are ignored.
pub fn read_inbox_in(dir: &Path) -> Vec<(PathBuf, Message)> {
    let inbox = dir.join("inbox");
    let Ok(entries) = fs::read_dir(&inbox) else {
        return Vec::new();
    };
    // `.take(SCAN_CAP)` is applied to the raw directory entries — BEFORE the
    // `.json` filter — so a hostile writer can't force an unbounded scan by
    // dropping a flood of non-json files that all survive to be iterated.
    let mut paths: Vec<PathBuf> = entries
        .take(SCAN_CAP)
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        // Bound the read: a forged file could be huge, and this runs on the
        // UI thread every refresh.
        let too_big = fs::metadata(&path)
            .map(|m| m.len() > MAX_FILE_BYTES)
            .unwrap_or(true);
        let sound = if too_big {
            None
        } else {
            fs::read_to_string(&path)
                .ok()
                .and_then(|b| serde_json::from_str::<Message>(&b).ok())
                .filter(|msg| message_is_sound(&stem, msg))
        };
        match sound {
            Some(msg) => out.push((path, msg)),
            None => {
                let _ = fs::rename(&path, path.with_extension("bad"));
            }
        }
    }
    out
}

/// Read the default inbox.
pub fn read_inbox() -> Vec<(PathBuf, Message)> {
    match mail_dir() {
        Some(dir) => read_inbox_in(&dir),
        None => Vec::new(),
    }
}

/// A message leased for delivery. Carries the validated id (derived from the
/// trusted inbox *filename*, never the JSON payload) so downstream lifecycle
/// moves can't be steered by a forged `id` field.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub path: PathBuf,
    pub id: String,
}

/// Atomically lease a pending message for delivery. Renames
/// `inbox/<id>.json` → `claimed/<id>.<pid>.json`; the rename fails for every
/// racer but one (the source vanishes after the winner moves it), so exactly
/// one router can own a message even if two ADE instances are running.
pub fn claim(dir: &Path, inbox_path: &Path) -> Result<Claimed, String> {
    let stem = inbox_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "message path has no stem".to_string())?;
    if !is_valid_id(stem) {
        return Err("message id is not path-safe".to_string());
    }
    let id = stem.to_string();
    let claimed = dir.join("claimed");
    create_dir_private(&claimed)?;
    let dest = claimed.join(format!("{}.{}.json", id, std::process::id()));
    fs::rename(inbox_path, &dest).map_err(|e| format!("claim message: {}", e))?;
    Ok(Claimed { path: dest, id })
}

/// Record an attempted injection: move a claimed file into `archive/`. The
/// audit record survives; call this only after tmux accepted the keys.
pub fn archive(dir: &Path, claimed: &Claimed) -> Result<(), String> {
    move_into(dir, "archive", &claimed.path, &claimed.id)
}

/// Return a claimed message to the inbox for retry. Call this ONLY when the
/// send never happened (a pre-injection failure) — never after an ambiguous
/// partial send, which could double-submit.
pub fn requeue(dir: &Path, claimed: &Claimed) -> Result<(), String> {
    move_into(dir, "inbox", &claimed.path, &claimed.id)
}

/// Move an inbox message to `deadletter/` (expired / undeliverable). `id`
/// must be the message's path-safe id (the caller derives it from the
/// trusted filename).
pub fn dead_letter(dir: &Path, path: &Path, id: &str) -> Result<(), String> {
    move_into(dir, "deadletter", path, id)
}

/// Recover claims stranded by a crashed router. A claim is meant to complete
/// within one keypress (synchronous send-keys), so anything sitting in
/// `claimed/` past `stale_after` is from a crash. We move it to
/// `deadletter/`, NOT back to the inbox: a stranded claim is ambiguous (the
/// send may or may not have landed), and auto-requeueing could double-submit.
/// Returns how many were recovered. `id` is re-derived from the trusted
/// filename (`<id>.<pid>`), so a forged payload can't redirect the move.
pub fn recover_stale_claims(dir: &Path, stale_after: std::time::Duration) -> usize {
    let claimed = dir.join("claimed");
    let Ok(entries) = fs::read_dir(&claimed) else {
        return 0;
    };
    let mut recovered = 0;
    for entry in entries.take(SCAN_CAP).flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age > stale_after)
            .unwrap_or(false);
        if !stale {
            continue;
        }
        // Claimed filename is `<id>.<pid>` before the `.json`; the id is the
        // part before the last dot and must be path-safe.
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let id = stem.rsplit_once('.').map(|(id, _)| id).unwrap_or(stem);
        if !is_valid_id(id) {
            // Not one of ours / unsafe — quarantine rather than move by id.
            let _ = fs::rename(&path, path.with_extension("bad"));
            continue;
        }
        if move_into(dir, "deadletter", &path, id).is_ok() {
            recovered += 1;
        }
    }
    recovered
}

fn move_into(dir: &Path, sub: &str, path: &Path, id: &str) -> Result<(), String> {
    if !is_valid_id(id) {
        return Err(format!("refusing to move message with unsafe id {:?}", id));
    }
    let target_dir = dir.join(sub);
    create_dir_private(&target_dir)?;
    let dest = target_dir.join(format!("{}.json", id));
    fs::rename(path, &dest).map_err(|e| format!("move message to {}: {}", sub, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway unique directory under the system temp dir. Avoids a
    /// tempfile dependency (ADE has none) while keeping tests hermetic.
    fn tmp_base() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "ade-mail-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tmp_base();
        let id = write_message_in(&dir, "a", "$1", "b", "local", "1:$2", "hello there").unwrap();
        let inbox = read_inbox_in(&dir);
        assert_eq!(inbox.len(), 1);
        let (_, msg) = &inbox[0];
        assert_eq!(msg.id, id);
        assert_eq!(msg.from_session, "a");
        assert_eq!(msg.to_session, "b");
        assert_eq!(msg.to_session_addr, "1:$2");
        assert_eq!(msg.body, "hello there");
    }

    #[test]
    fn read_inbox_is_publish_ordered() {
        let dir = tmp_base();
        write_message_in(&dir, "a", "$1", "b", "local", "1:$2", "first").unwrap();
        write_message_in(&dir, "a", "$1", "b", "local", "1:$2", "second").unwrap();
        write_message_in(&dir, "a", "$1", "b", "local", "1:$2", "third").unwrap();
        let bodies: Vec<_> = read_inbox_in(&dir)
            .into_iter()
            .map(|(_, m)| m.body)
            .collect();
        assert_eq!(bodies, vec!["first", "second", "third"]);
    }

    #[test]
    fn malformed_files_are_quarantined_not_returned() {
        let dir = tmp_base();
        let inbox = dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("000-0-0.json"), "{ not valid json").unwrap();
        assert!(read_inbox_in(&dir).is_empty());
        assert!(inbox.join("000-0-0.bad").exists());
    }

    #[test]
    fn id_not_matching_filename_is_quarantined() {
        // The payload id is what lifecycle moves are keyed on, so it must not
        // be allowed to disagree with the filename it arrived under.
        let dir = tmp_base();
        let inbox = dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        let forged = r#"{"id":"999-9-9","from_session":"x","from_session_id":"$1","to_session":"y","to_session_addr":"1:$2","body":"harmless","created_millis":0}"#;
        fs::write(inbox.join("000-0-0.json"), forged).unwrap();
        assert!(read_inbox_in(&dir).is_empty());
        assert!(inbox.join("000-0-0.bad").exists());
    }

    #[test]
    fn body_failing_validation_is_quarantined() {
        // A file written directly to the inbox bypasses the CLI's validation,
        // so the reader must re-apply it rather than trust the payload.
        let dir = tmp_base();
        let inbox = dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        let forged = r#"{"id":"000-0-0","from_session":"x","from_session_id":"$1","to_session":"y","to_session_addr":"1:$2","body":"line1\nline2","created_millis":0}"#;
        fs::write(inbox.join("000-0-0.json"), forged).unwrap();
        assert!(read_inbox_in(&dir).is_empty());
        assert!(inbox.join("000-0-0.bad").exists());
    }

    #[test]
    fn forged_sender_making_the_rendered_line_oversized_is_quarantined() {
        // Validation must cover the *rendered* injection, not just the body:
        // `from_session` is interpolated into it and is equally untrusted.
        let dir = tmp_base();
        let inbox = dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        let huge_sender = "s".repeat(MAX_BODY_BYTES + 512);
        let forged = format!(
            r#"{{"id":"000-0-0","from_session":"{}","from_session_id":"$1","to_session":"y","to_session_addr":"1:$2","body":"ok","created_millis":0}}"#,
            huge_sender
        );
        fs::write(inbox.join("000-0-0.json"), forged).unwrap();
        assert!(read_inbox_in(&dir).is_empty());
        assert!(inbox.join("000-0-0.bad").exists());
    }

    #[test]
    fn oversized_files_are_quarantined() {
        let dir = tmp_base();
        let inbox = dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("000-0-0.json"), "x".repeat((MAX_FILE_BYTES + 1) as usize))
            .unwrap();
        assert!(read_inbox_in(&dir).is_empty());
        assert!(inbox.join("000-0-0.bad").exists());
    }

    #[test]
    fn validate_body_rejects_control_and_spoofing_chars() {
        assert!(validate_body("ok text").is_ok());
        assert!(validate_body("has\nnewline").is_err());
        assert!(validate_body("has\ttab").is_err());
        assert!(validate_body("esc\x1b[0m").is_err());
        assert!(validate_body("bidi\u{202E}override").is_err());
        assert!(validate_body("zero\u{200B}width").is_err());
        assert!(validate_body("").is_err());
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(validate_body(&big).is_err());
    }

    #[test]
    fn is_valid_id_blocks_path_traversal() {
        assert!(is_valid_id("001785-123-0000"));
        assert!(!is_valid_id("../../etc/passwd"));
        assert!(!is_valid_id("a/b"));
        assert!(!is_valid_id("id.with.dots"));
        assert!(!is_valid_id(""));
    }

    #[test]
    fn move_into_refuses_unsafe_id() {
        let dir = tmp_base();
        let inbox = dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        let victim = inbox.join("x.json");
        fs::write(&victim, "{}").unwrap();
        // A traversal id must be refused rather than renaming out of the tree.
        assert!(dead_letter(&dir, &victim, "../../escape").is_err());
        assert!(victim.exists());
    }

    #[test]
    fn write_rejects_bad_body_self_send_and_missing_recipient_addr() {
        let dir = tmp_base();
        assert!(write_message_in(&dir, "a", "$1", "b", "local", "1:$2", "bad\nbody").is_err());
        assert!(write_message_in(&dir, "a", "$1", "a", "local", "$1", "to self").is_err());
        // An unresolvable recipient id must refuse — delivery can't be
        // verified without it.
        assert!(write_message_in(&dir, "a", "$1", "b", "local", "", "no recipient id").is_err());
        assert!(read_inbox_in(&dir).is_empty());
    }

    #[test]
    fn forged_sender_with_control_chars_is_quarantined() {
        let dir = tmp_base();
        let inbox = dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        // Valid body + id, but an ESC smuggled into from_session would land
        // in the rendered injection — must be quarantined at read time. Build
        // the ESC in Rust so no control byte lives in this source file.
        let forged = format!(
            "{{\"id\":\"000-0-0\",\"from_session\":\"evil{esc}[2J\",\"from_session_id\":\"$9\",\"to_session\":\"b\",\"to_session_addr\":\"1:$2\",\"body\":\"hi\",\"created_millis\":0}}",
            esc = '\u{1b}'
        );
        fs::write(inbox.join("000-0-0.json"), forged).unwrap();
        assert!(read_inbox_in(&dir).is_empty());
        assert!(inbox.join("000-0-0.bad").exists());
    }

    #[test]
    fn per_sender_cap_is_enforced() {
        let dir = tmp_base();
        for i in 0..MAX_PER_SENDER {
            write_message_in(&dir, "spammer", "$1", "b", "local", "1:$2", &format!("m{}", i)).unwrap();
        }
        let over = write_message_in(&dir, "spammer", "$1", "b", "local", "1:$2", "one too many");
        assert!(over.is_err());
        assert!(write_message_in(&dir, "other", "$3", "b", "local", "1:$2", "fine").is_ok());
    }

    #[test]
    fn concurrent_claimers_produce_exactly_one_winner() {
        // Two routers (or two ADE instances) racing on the same message must
        // resolve to exactly one delivery. The rename is the arbiter, so the
        // threads are released together by a barrier to make them actually
        // contend rather than run in sequence.
        use std::sync::{Arc, Barrier};

        let dir = Arc::new(tmp_base());
        write_message_in(&dir, "a", "$1", "b", "local", "1:$2", "deliver me").unwrap();
        let (path, _) = read_inbox_in(&dir).into_iter().next().unwrap();
        let path = Arc::new(path);

        const RACERS: usize = 8;
        let barrier = Arc::new(Barrier::new(RACERS));
        let handles: Vec<_> = (0..RACERS)
            .map(|_| {
                let dir = Arc::clone(&dir);
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    claim(&dir, &path).is_ok()
                })
            })
            .collect();
        let winners = handles
            .into_iter()
            .filter(|_| true)
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();

        assert_eq!(winners, 1, "exactly one claimer may win the race");
        assert!(read_inbox_in(&dir).is_empty(), "message left the inbox");
        assert_eq!(
            fs::read_dir(dir.join("claimed")).unwrap().count(),
            1,
            "exactly one claim file must exist"
        );
    }

    #[test]
    fn archive_leaves_an_audit_record() {
        let dir = tmp_base();
        write_message_in(&dir, "a", "$1", "b", "local", "1:$2", "deliver me").unwrap();
        let (path, _) = read_inbox_in(&dir).into_iter().next().unwrap();
        let claimed = claim(&dir, &path).unwrap();
        assert!(claimed.path.exists());
        assert!(!path.exists());
        archive(&dir, &claimed).unwrap();
        assert!(dir
            .join("archive")
            .join(format!("{}.json", claimed.id))
            .exists());
    }

    #[test]
    fn requeue_returns_a_claim_to_the_inbox() {
        let dir = tmp_base();
        write_message_in(&dir, "a", "$1", "b", "local", "1:$2", "retry me").unwrap();
        let (path, _) = read_inbox_in(&dir).into_iter().next().unwrap();
        let claimed = claim(&dir, &path).unwrap();
        requeue(&dir, &claimed).unwrap();
        let back = read_inbox_in(&dir);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].1.body, "retry me");
    }

    #[test]
    fn stale_claims_go_to_deadletter_not_inbox() {
        // Both directions of the age comparison are pinned: a fresh claim must
        // be left alone, and only one that has actually aged past the threshold
        // is recovered. (A zero threshold would pass even if the comparison
        // were inverted, so a real interval is used.)
        let dir = tmp_base();
        write_message_in(&dir, "a", "$1", "b", "local", "1:$2", "stranded").unwrap();
        let (path, _) = read_inbox_in(&dir).into_iter().next().unwrap();
        let claimed = claim(&dir, &path).unwrap();

        // Fresh: a generous threshold must not touch it.
        assert_eq!(
            recover_stale_claims(&dir, std::time::Duration::from_secs(3600)),
            0,
            "a fresh claim must not be recovered"
        );
        assert!(claimed.path.exists());

        // Aged past a short threshold: recovered.
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert_eq!(
            recover_stale_claims(&dir, std::time::Duration::from_millis(50)),
            1
        );

        // Recovered to deadletter, NEVER silently back to the inbox — the send
        // may already have landed, so re-delivering could double-submit.
        assert!(read_inbox_in(&dir).is_empty());
        assert!(!claimed.path.exists());
        assert!(dir
            .join("deadletter")
            .join(format!("{}.json", claimed.id))
            .exists());
    }

    #[test]
    fn inbox_scan_is_bounded_by_scan_cap() {
        // `MAX_TOTAL` only binds cooperative writers; a hostile same-UID
        // process can drop arbitrarily many files, and the scan runs on the UI
        // thread every refresh. The cap applies to raw directory entries.
        let dir = tmp_base();
        let inbox = dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        for i in 0..(SCAN_CAP + 64) {
            let id = format!("{:015}-0-{:04}", i, i);
            let body = format!(
                r#"{{"id":"{}","from_session":"a","from_session_id":"$1","to_session":"b","to_session_addr":"1:$2","body":"x","created_millis":0}}"#,
                id
            );
            fs::write(inbox.join(format!("{}.json", id)), body).unwrap();
        }
        assert!(read_inbox_in(&dir).len() <= SCAN_CAP);
    }

    #[test]
    fn total_cap_is_enforced_across_senders() {
        let dir = tmp_base();
        let inbox = dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        // Pre-fill to the total cap with valid messages from many senders.
        for i in 0..MAX_TOTAL {
            let id = format!("{:015}-0-{:04}", i, i);
            let body = format!(
                r#"{{"id":"{}","from_session":"s{}","from_session_id":"$1","to_session":"b","to_session_addr":"1:$2","body":"x","created_millis":0}}"#,
                id, i
            );
            fs::write(inbox.join(format!("{}.json", id)), body).unwrap();
        }
        let over = write_message_in(&dir, "fresh", "$9", "b", "local", "1:$2", "one too many");
        assert!(over.is_err(), "total cap must refuse further sends");
    }

    #[test]
    #[test]
    fn rejects_every_invisible_format_character() {
        // Codex named U+2060 and U+061C specifically: both passed the old
        // hand-picked list and can make a body render as something other than
        // what was sent.
        for bad in ['\u{2060}', '\u{061C}', '\u{00AD}', '\u{200B}', '\u{202E}',
                    '\u{FEFF}', '\u{2066}', '\u{E0041}'] {
            let body = format!("ok{}hidden", bad);
            assert!(
                validate_body(&body).is_err(),
                "U+{:04X} must be rejected",
                bad as u32
            );
            assert!(validate_injection(&body).is_err(), "U+{:04X}", bad as u32);
        }
    }

    #[test]
    fn ordinary_text_and_real_punctuation_still_pass() {
        // The widened rule must not start rejecting normal messages.
        for good in [
            "build is green",
            "see PR #28 — merged, 371 tests",
            "path: /Users/x/Dev/archie/archie-monorepo",
            "naïve café — 日本語 emoji 🚀 ok",
        ] {
            assert!(validate_body(good).is_ok(), "{:?} must be allowed", good);
        }
    }

    fn render_injection_prefixes_sender() {
        let dir = tmp_base();
        write_message_in(&dir, "worker/api", "$7", "b", "local", "1:$2", "build is green").unwrap();
        let (_, msg) = read_inbox_in(&dir).into_iter().next().unwrap();
        assert_eq!(
            msg.render_injection(),
            "[ADE mail from worker/api] build is green"
        );
    }
}
