//! Mail **delivery policy** — the decisions and state machine that sit between
//! the mailbox (`mail.rs`) and the tmux side effects (`tmux::TmuxBackend`).
//!
//! Everything here is deliberately free of process-wide I/O so it can be tested
//! exhaustively: the gate matrix is a pure function over observed facts, and the
//! claim → send → archive/requeue machine talks to a `DeliveryEnv` trait that
//! tests substitute with a fake (Codex review). `App` supplies the real,
//! tmux-backed adapter and owns the UI wiring.
//!
//! The one heuristic in here is `composer_state`, which reads the recipient's
//! rendered composer to avoid typing on top of a human's unsent draft.

use std::path::Path;

use crate::claude_status::ClaudeState;
use crate::mail::{self, Claimed, Message};
use crate::tmux::SendTextError;

/// What the recipient's Claude composer looks like right before we inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerState {
    /// Prompt found and nothing typed — safe to inject.
    Empty,
    /// Prompt found with text already in it — a human's unsent draft.
    /// Injecting would append to it and submit both together.
    Draft,
    /// Claude is showing its first-run workspace-trust dialog instead of a
    /// composer ("Do you trust the files in this folder?"). Refuses like
    /// `Unknown`, but it is worth its own variant because the cause is
    /// specific, common for freshly spawned sessions, and has an obvious fix —
    /// and because that screen contains a prompt glyph, so a looser reading
    /// could "find a prompt" and answer a security dialog.
    AwaitingTrust,
    /// No prompt line found, or the pane couldn't be read. We can't prove the
    /// composer is empty, so callers must treat this as unsafe (fail-closed).
    Unknown,
}

/// Markers for Claude's first-run workspace-trust screen. Matched
/// case-insensitively against the whole capture. Kept to short, stable phrases
/// rather than the full layout: a miss only costs us a vaguer message (we still
/// refuse via `Unknown`), so over-fitting to the exact wording would be worse
/// than matching loosely.
const TRUST_MARKERS: &[&str] = &[
    "trust the files in this folder",
    "trust this folder",
    "do you trust",
];

/// Claude Code's composer prompt marker.
const COMPOSER_PROMPT: char = '\u{276F}';

/// Minimum run of horizontal box-drawing characters that counts as one of the
/// rules framing Claude's composer.
const RULE_MIN_RUN: usize = 20;

/// True if `line` is one of the horizontal rules that frame the composer.
fn is_frame_rule(line: &str) -> bool {
    let t = line.trim();
    if t.chars().count() < RULE_MIN_RUN {
        return false;
    }
    t.chars().all(|c| matches!(c, '\u{2500}' | '\u{2501}' | '\u{2504}' | '\u{2505}' | '-' | '\u{2550}'))
}

/// Strip SGR escape sequences from a captured line, returning the visible text
/// alongside a per-character flag for whether it was rendered **dim** (SGR 2).
///
/// The dim flag is load-bearing: Claude renders a history/placeholder hint in an
/// *empty* composer using dim text, which is visually and structurally identical
/// to a real draft once styling is discarded. Treating that hint as a draft
/// makes the composer guard refuse delivery forever, so the classifier needs the
/// styling to tell them apart.
fn strip_sgr_with_dim(line: &str) -> (String, Vec<bool>) {
    let mut text = String::new();
    let mut dim_mask = Vec::new();
    let mut dim = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            text.push(c);
            dim_mask.push(dim);
            continue;
        }
        // ESC [ params letter — only SGR ('m') affects styling; skip the rest.
        if chars.peek() != Some(&'[') {
            continue;
        }
        chars.next();
        let mut params = String::new();
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                if c == 'm' {
                    for p in params.split(';') {
                        match p.trim() {
                            "2" => dim = true,
                            // 0 = reset all, 22 = normal intensity
                            "0" | "" | "22" => dim = false,
                            _ => {}
                        }
                    }
                }
                break;
            }
            params.push(c);
        }
    }
    (text, dim_mask)
}

/// Classify a Claude pane's composer from its visible content.
///
/// Claude frames its input box between two horizontal rules, with the prompt
/// glyph on the first row:
///
/// ```text
/// ────────────────────────────────
/// > <nbsp>maybe some typed text
/// ────────────────────────────────
///   status line
/// ```
///
/// To return `Empty` we require the **whole frame** to be present and every row
/// inside it to be blank. Anything else — no frame, no prompt, content on the
/// prompt row, content on a continuation row, or a layout we don't recognise —
/// is `Draft` or `Unknown`, both of which refuse delivery.
///
/// The asymmetry is deliberate and is the whole point of this function. A false
/// `Draft`/`Unknown` merely declines one delivery, with the reason shown to the
/// user. A false `Empty` types a message on top of somebody's half-written
/// prompt and submits both together, which is unrecoverable. So every ambiguity
/// resolves away from `Empty`.
///
/// Notes on details that look fussy but are load-bearing:
///  * The glyph is followed by U+00A0 (nbsp), not a space, in real output.
///    `trim()` handles it because nbsp is Unicode whitespace.
///  * Trailing `|`/`\u{2502}` characters are NOT stripped: a draft consisting of
///    `||` is real typed text, and stripping "borders" would erase it
///    (Codex review).
///  * A bare prompt row is not sufficient evidence of emptiness — a wrapped or
///    multi-line draft continues on the rows beneath it, so those are checked
///    too.
///
/// This is a heuristic over rendered output; there is no API to ask Claude
/// whether its composer is dirty. It is pinned by table-driven tests built from
/// real captures (`composer_state_tests`).
pub fn composer_state(capture: &str) -> ComposerState {
    // Parse once: visible text for structure, dim flags for the content check.
    let parsed: Vec<(String, Vec<bool>)> =
        capture.lines().map(strip_sgr_with_dim).collect();
    let lines: Vec<&str> = parsed.iter().map(|(t, _)| t.as_str()).collect();

    // A modal first-run screen is checked first: it renders its own `\u{276F}`
    // selector ("> 1. Yes, I trust this folder"), so structural parsing below
    // would call it Unknown and lose the reason.
    //
    // Matched against the SGR-stripped text, not the raw capture: the screen
    // styles part of the phrase, so escape sequences land mid-sentence and a
    // raw substring search silently misses.
    let lowered = lines.join("\n").to_lowercase();
    if TRUST_MARKERS.iter().any(|m| lowered.contains(m)) {
        return ComposerState::AwaitingTrust;
    }

    let prompt_idx = lines.iter().rposition(|line| match line.find(COMPOSER_PROMPT) {
        Some(idx) => line[..idx].chars().all(char::is_whitespace),
        None => false,
    });
    let Some(prompt_idx) = prompt_idx else {
        return ComposerState::Unknown;
    };
    let prompt_line = lines[prompt_idx];

    // Frame must open directly above and close below, or we don't recognise the
    // layout and must not claim emptiness.
    let opened = lines[..prompt_idx]
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| is_frame_rule(l))
        .unwrap_or(false);
    if !opened {
        return ComposerState::Unknown;
    }
    let mut closing = None;
    for (offset, line) in lines[prompt_idx + 1..].iter().enumerate() {
        if is_frame_rule(line) {
            closing = Some(prompt_idx + 1 + offset);
            break;
        }
    }
    let Some(closing) = closing else {
        return ComposerState::Unknown;
    };

    // Content on the prompt row. Dim-only content is Claude's placeholder hint
    // shown in an EMPTY composer — typing replaces it rather than appending —
    // so it must not be mistaken for the user's unsent text.
    let glyph_at = prompt_line.find(COMPOSER_PROMPT).unwrap();
    let content_from = glyph_at + COMPOSER_PROMPT.len_utf8();
    if !prompt_line[content_from..].trim().is_empty() {
        let dim_mask = &parsed[prompt_idx].1;
        // Map byte offset -> char index so the mask lines up with the text.
        let start_char = prompt_line[..content_from].chars().count();
        let all_dim = prompt_line[content_from..]
            .chars()
            .enumerate()
            .filter(|(_, c)| !c.is_whitespace())
            .all(|(i, _)| dim_mask.get(start_char + i).copied().unwrap_or(false));
        if !all_dim {
            return ComposerState::Draft;
        }
    }

    // Continuation rows inside the frame: same rule.
    for (row, line) in lines
        .iter()
        .enumerate()
        .take(closing)
        .skip(prompt_idx + 1)
    {
        if line.trim().is_empty() {
            continue;
        }
        let dim_mask = &parsed[row].1;
        let all_dim = line
            .chars()
            .enumerate()
            .filter(|(_, c)| !c.is_whitespace())
            .all(|(i, _)| dim_mask.get(i).copied().unwrap_or(false));
        if !all_dim {
            return ComposerState::Draft;
        }
    }
    ComposerState::Empty
}


/// The side effects delivery needs, behind a seam so tests can fake them.
/// The production adapter (`app::TmuxDelivery`) shells out to tmux.
pub trait DeliveryEnv {
    /// Live lookup of a local session: `(address, pane_ids)` where address is
    /// `"<server-pid>:<session_id>"` and pane ids cover ALL windows. `None` if
    /// the session can't be inspected (gone, tmux unavailable).
    fn live_session(&self, name: &str) -> Option<(String, Vec<String>)>;
    /// Classify the composer of an exact pane.
    fn composer(&self, pane_id: &str) -> ComposerState;
    /// Type `text` into an exact pane and submit it.
    fn send_text(&self, pane_id: &str, text: &str) -> Result<(), SendTextError>;
}

/// What ADE knew about the selected session from the last refresh, plus the
/// address the queued message recorded. Cheap facts, gathered before any tmux
/// call, so obvious refusals cost nothing.
#[derive(Debug, Clone)]
pub struct DeliveryRequest<'a> {
    pub session_name: &'a str,
    pub is_local: bool,
    pub has_pending_mail: bool,
    pub claude_present: bool,
    pub claude: Option<ClaudeState>,
}

/// Outcome of the gate: either an exact pane to type into, or a refusal with a
/// user-facing reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Deliver { pane_id: String },
    Refuse(String),
}

/// Decide whether a message may be injected, and into which pane.
///
/// Ordering matters and is part of the contract: the cheap in-memory checks run
/// before anything shells out, and every uncertainty refuses. `recorded_addr` is
/// the address captured when the message was queued; it must still match the
/// live session or the mail belongs to a different conversation that merely
/// shares the name.
pub fn gate<'a>(
    env: &dyn DeliveryEnv,
    req: &DeliveryRequest<'a>,
    recorded_addr: &str,
) -> Gate {
    let name = req.session_name;

    // `is_local` is no longer a refusal. Remote delivery is supported, and the
    // machine travels with the env (`app::Delivery`) rather than being a policy
    // decision here — so the gate and the injection cannot disagree about where
    // the recipient is.
    //
    // It still earns its place in the messages. When mail does not arrive, the
    // first thing worth knowing is whether the gate was even looking at the
    // right machine: a refusal naming a session that "has no live Claude" reads
    // very differently once you know it was inspected over SSH.
    let whence = if req.is_local { "" } else { " (on a remote host)" };
    if !req.has_pending_mail {
        return Gate::Refuse(format!("no pending mail for '{}'", name));
    }
    // Only an idle Claude at its prompt is a valid target. ADE models that as
    // "a Claude pane exists" AND "no active state". Working and
    // AwaitingApproval must never be interrupted; a defensive `Some(Idle)`
    // (never produced by the rollup) is treated as unproven.
    if !req.claude_present {
        return Gate::Refuse(format!(
            "'{}'{} has no live Claude session to deliver into",
            name, whence
        ));
    }
    match req.claude {
        None => {}
        Some(ClaudeState::Working) => {
            return Gate::Refuse(format!(
                "'{}' is working — deliver once it's idle at its prompt",
                name
            ))
        }
        Some(ClaudeState::AwaitingApproval) => {
            return Gate::Refuse(format!(
                "'{}' is at a permission prompt — deliver once it's idle \
                 (avoid answering the prompt)",
                name
            ))
        }
        Some(ClaudeState::Idle) => {
            return Gate::Refuse(format!("'{}' isn't confirmed idle — try again", name))
        }
    }

    let Some((live_addr, pane_ids)) = env.live_session(name) else {
        // For a remote recipient this is also what an unreachable host looks
        // like, which is worth saying rather than implying the session is gone.
        return Gate::Refuse(format!(
            "could not inspect panes of '{}'{}",
            name, whence
        ));
    };

    // Exactly one pane, or we can't know which one holds Claude. (A second
    // window can hide a shell, which is why the lookup is session-scoped.)
    let pane_id = match pane_ids.len() {
        1 => pane_ids.into_iter().next().unwrap(),
        n => {
            return Gate::Refuse(format!(
                "'{}' has {} panes — mail delivery needs a single-pane session \
                 so the message lands in Claude's pane",
                name, n
            ))
        }
    };

    // Identity, fail-closed: an empty recorded address is unverifiable.
    if recorded_addr.is_empty() || recorded_addr != live_addr {
        return Gate::Refuse(format!(
            "'{}' isn't the session this message was addressed to (recreated?) \
             — not delivering",
            name
        ));
    }

    // Finally: never type on top of a human's unsent draft.
    match env.composer(&pane_id) {
        ComposerState::Empty => Gate::Deliver { pane_id },
        ComposerState::Draft => Gate::Refuse(format!(
            "'{}' has unsent text at its prompt — not delivering on top of it. \
             Clear or submit that draft, then press m again.",
            name
        )),
        ComposerState::AwaitingTrust => Gate::Refuse(format!(
            "'{}' is waiting on Claude's workspace-trust prompt and hasn't \
             started yet — accept it once in that session (or spawn into an \
             already-trusted directory); ADE will not answer a security prompt \
             for you",
            name
        )),
        ComposerState::Unknown => Gate::Refuse(format!(
            "could not confirm '{}' is sitting empty at its prompt — not delivering",
            name
        )),
    }
}

/// What happened when a claimed message was actually pushed at a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Keys accepted and the audit record filed.
    Delivered,
    /// Keys accepted but the archive move failed. Deliberately NOT requeued —
    /// the text already landed, so retrying would type it twice.
    DeliveredButNotArchived(String),
    /// Nothing was typed; the message is back in the inbox for another try.
    Requeued(String),
    /// Nothing was typed, and returning it to the inbox also failed.
    RequeueFailed { send: String, requeue: String },
    /// The send failed *after* dispatch, so text may or may not have landed.
    /// Left claimed for stale-claim recovery to dead-letter; never retried.
    Ambiguous(String),
}

/// Run the claim → send → archive/requeue machine for one already-claimed
/// message. `dir` is the mailbox root, passed explicitly so tests don't touch
/// the real one.
///
/// The rules this encodes are the at-most-once policy: requeue only when we
/// are certain nothing was typed, and treat every other failure as terminal
/// rather than risk a duplicate injection.
pub fn execute(
    env: &dyn DeliveryEnv,
    dir: &Path,
    claimed: &Claimed,
    msg: &Message,
    pane_id: &str,
) -> Outcome {
    match env.send_text(pane_id, &msg.render_injection()) {
        Ok(()) => match mail::archive(dir, claimed) {
            Ok(()) => Outcome::Delivered,
            Err(e) => Outcome::DeliveredButNotArchived(e),
        },
        Err(e) if e.is_ambiguous() => Outcome::Ambiguous(e.message),
        Err(e) => match mail::requeue(dir, claimed) {
            Ok(()) => Outcome::Requeued(e.message),
            Err(re) => Outcome::RequeueFailed {
                send: e.message,
                requeue: re,
            },
        },
    }
}

/// Why a queued message is sitting there, as far as ADE can tell **without**
/// running a tmux query.
///
/// The router logs an exact reason when it tries to deliver, but nobody reads a
/// router log — the operator watching the TUI needs to know that mail is stuck
/// and roughly why. This derives what it can from the session state already
/// held in memory, so it costs nothing per refresh. The precise reason (draft
/// in the composer, workspace trust, multi-pane, identity mismatch) needs a
/// pane probe and is surfaced when the user actually presses `m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldReason {
    /// Claude is mid-turn; delivery waits for the turn to end. Expected, brief.
    Working,
    /// Sitting at a permission prompt — ADE will not type there.
    AwaitingApproval,
    /// No Claude in that session at all; nothing will ever pick this up until
    /// one is started.
    NoClaude,
    /// Nothing in the cheap state explains it — the blocker is something only a
    /// pane probe can see (an unsent draft, the workspace-trust screen, more
    /// than one pane, or a recreated session).
    NeedsProbe,
}

impl HeldReason {
    /// From the session facts ADE already refreshes every ~2s.
    pub fn from_session_state(
        claude_present: bool,
        claude: Option<ClaudeState>,
    ) -> Self {
        if !claude_present {
            return HeldReason::NoClaude;
        }
        match claude {
            Some(ClaudeState::Working) => HeldReason::Working,
            Some(ClaudeState::AwaitingApproval) => HeldReason::AwaitingApproval,
            _ => HeldReason::NeedsProbe,
        }
    }

    /// Compact label for the session row.
    pub fn short_label(&self) -> &'static str {
        match self {
            HeldReason::Working => "busy",
            HeldReason::AwaitingApproval => "approve",
            HeldReason::NoClaude => "no claude",
            HeldReason::NeedsProbe => "blocked",
        }
    }

    /// Whether this is a normal transient wait rather than something the
    /// operator probably has to act on. A session that is merely mid-turn will
    /// clear on its own; the others generally will not.
    pub fn is_transient(&self) -> bool {
        matches!(self, HeldReason::Working)
    }
}

/// How long a message may wait before the UI calls it *held* rather than merely
/// pending. Long enough that an ordinary turn (and the ~2s router cadence)
/// doesn't trip it, short enough that a real stall is visible quickly.
pub const HELD_AFTER: std::time::Duration = std::time::Duration::from_secs(90);

/// Render a wait as a compact age for the session row: `45s`, `12m`, `3h`, `2d`.
pub fn format_wait(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod composer_state_tests {
    //! Table-driven tests for `composer_state`, built from real
    //! `tmux capture-pane` output of a live Claude Code pane.
    //!
    //! The contract under test is asymmetric: `Empty` is a promise that it is
    //! safe to type into the pane, so every ambiguous or unrecognised layout
    //! must come back `Draft` or `Unknown`. Cases marked "dangerous direction"
    //! are the ones where a bug would silently clobber a human's typing.
    use super::{composer_state, ComposerState};

    const RULE: &str = "────────────────────────────────────────────────────────";

    /// Build a capture with the given rows inside the composer frame.
    fn framed(rows: &[&str]) -> String {
        let mut s = String::from("⏺ some earlier assistant output\n\n");
        s.push_str(RULE);
        s.push('\n');
        for r in rows {
            s.push_str(r);
            s.push('\n');
        }
        s.push_str(RULE);
        s.push('\n');
        s.push_str("  ADE main\n  ⏵⏵ auto mode on (shift+tab to cycle)\n");
        s
    }

    #[test]
    fn empty_composer_with_nbsp_is_empty() {
        // Exactly what a real idle pane renders: glyph + U+00A0.
        assert_eq!(composer_state(&framed(&["❯\u{a0}"])), ComposerState::Empty);
    }

    #[test]
    fn plain_draft_is_draft() {
        assert_eq!(
            composer_state(&framed(&["❯\u{a0}commit the mail feature"])),
            ComposerState::Draft
        );
    }

    #[test]
    fn pipe_only_draft_is_draft_not_empty() {
        // DANGEROUS DIRECTION: an earlier version stripped trailing pipe-like
        // characters as "box borders", which erased these real drafts.
        for body in ["|", "||", "│", "┃", "| |", "a|"] {
            let cap = framed(&[&format!("❯\u{a0}{}", body)]);
            assert_eq!(
                composer_state(&cap),
                ComposerState::Draft,
                "body {:?} must count as a draft",
                body
            );
        }
    }

    #[test]
    fn multiline_draft_under_bare_prompt_is_draft_not_empty() {
        // DANGEROUS DIRECTION: the prompt row is blank but the draft continues
        // on the rows beneath it, still inside the frame.
        let cap = framed(&["❯\u{a0}", "  second line of the draft"]);
        assert_eq!(composer_state(&cap), ComposerState::Draft);
    }

    #[test]
    fn wrapped_draft_is_draft() {
        let cap = framed(&[
            "❯\u{a0}a very long prompt that wrapped across",
            "  more wrapped text here",
        ]);
        assert_eq!(composer_state(&cap), ComposerState::Draft);
    }

    #[test]
    fn blank_rows_inside_frame_still_empty() {
        assert_eq!(
            composer_state(&framed(&["❯\u{a0}", "   ", ""])),
            ComposerState::Empty
        );
    }

    #[test]
    fn transcript_prompt_without_frame_is_unknown() {
        // Submitted prompts are echoed into the transcript with the same glyph.
        // Seeing one of those with no composer frame below means we are not
        // looking at an idle composer at all.
        let cap = "❯\u{a0}an earlier submitted prompt\n⏺ the reply\n✻ Cooked for 3s\n";
        assert_eq!(composer_state(cap), ComposerState::Unknown);
    }

    #[test]
    fn no_prompt_glyph_at_all_is_unknown() {
        assert_eq!(composer_state("just some output\nand more\n"), ComposerState::Unknown);
        assert_eq!(composer_state(""), ComposerState::Unknown);
    }

    #[test]
    fn prompt_with_no_closing_rule_is_unknown() {
        let cap = format!("{}\n❯\u{a0}\n", RULE);
        assert_eq!(composer_state(&cap), ComposerState::Unknown);
    }

    #[test]
    fn prompt_with_no_opening_rule_is_unknown() {
        let cap = format!("some output\n❯\u{a0}\n{}\n", RULE);
        assert_eq!(composer_state(&cap), ComposerState::Unknown);
    }

    #[test]
    fn glyph_not_first_on_line_is_not_a_prompt_row() {
        // A transcript line mentioning the glyph mid-sentence must not be
        // mistaken for the composer.
        // Realistic layout: the transcript mention sits ABOVE the frame, the
        // composer row inside it.
        let cap = format!(
            "⏺ use ❯ to submit\n{}\n❯\u{a0}\n{}\n  ADE main\n",
            RULE, RULE
        );
        assert_eq!(composer_state(&cap), ComposerState::Empty);
        // But if that mid-sentence line were the only candidate, no frame
        // encloses it as a prompt row → not Empty.
        let cap2 = "⏺ use ❯ to submit\nmore text\n";
        assert_eq!(composer_state(cap2), ComposerState::Unknown);
    }

    #[test]
    fn real_capture_empty_and_draft_round_trip() {
        // Verbatim shapes observed via `tmux capture-pane -p` during live
        // testing (rules truncated for width).
        let empty = format!(
            "⏺ BUSY-OK\n✻ Baked for 2s\n{}\n❯\u{a0}\n{}\n  ADE main\n",
            RULE, RULE
        );
        assert_eq!(composer_state(&empty), ComposerState::Empty);

        let draft = format!(
            "⏺ BUSY-OK\n✻ Baked for 2s\n{}\n❯\u{a0}check the inbox\n{}\n  ADE main\n",
            RULE, RULE
        );
        assert_eq!(composer_state(&draft), ComposerState::Draft);
    }


    // ── first-run workspace-trust screen ──
    //
    // Verbatim from a session spawned into an untrusted directory on the
    // server. Note it renders its own selector glyph, so the classifier must
    // recognise the screen rather than trying to parse a composer out of it.

    const TRUST_SCREEN: &str = "\
 Claude Code'll be able to read, edit, and execute files here.

 Security guide

 \u{276F} 1. Yes, I trust this folder
   2. No, exit

 Enter to confirm \u{b7} Esc to cancel
";

    #[test]
    fn workspace_trust_screen_is_reported_as_such() {
        assert_eq!(composer_state(TRUST_SCREEN), ComposerState::AwaitingTrust);
    }

    #[test]
    fn trust_screen_is_never_mistaken_for_a_usable_prompt() {
        // The failure that matters: treating that `>` selector as a composer
        // would type into — and submit — a security dialog.
        let st = composer_state(TRUST_SCREEN);
        assert_ne!(st, ComposerState::Empty, "must never look deliverable");
        assert_ne!(st, ComposerState::Draft);
    }

    #[test]
    fn trust_screen_is_recognised_through_ansi_styling() {
        // The real screen highlights part of the line, so escape sequences sit
        // mid-phrase. Matching the raw capture misses it; matching the stripped
        // text must not. This exact gap shipped once and was caught live.
        let styled = " \u{1b}[7m\u{276F} 1. Yes, I trust \u{1b}[1mthis folder\u{1b}[0m\n   2. No, exit\n";
        assert_eq!(composer_state(styled), ComposerState::AwaitingTrust);
    }

    #[test]
    fn trust_wording_variants_are_recognised() {
        for phrase in [
            "Do you trust the files in this folder?",
            "1. Yes, I trust this folder",
            "do you trust",
        ] {
            let cap = format!("{}\n\u{276F} 1. Yes\n", phrase);
            assert_eq!(
                composer_state(&cap),
                ComposerState::AwaitingTrust,
                "phrase {:?} should be recognised",
                phrase
            );
        }
    }

    #[test]
    fn an_ordinary_composer_is_not_flagged_as_trust() {
        // Guard against the markers being so loose they swallow normal panes.
        assert_eq!(composer_state(&framed(&["\u{276F}\u{a0}"])), ComposerState::Empty);
        assert_eq!(
            composer_state(&framed(&["\u{276F}\u{a0}should I trust this library?"])),
            ComposerState::Draft,
            "a draft merely mentioning trust is still a draft"
        );
    }

    // ── dim placeholder hint vs. a real draft ──
    //
    // Claude renders a history hint in an EMPTY composer using dim text (SGR 2).
    // Stripped of styling it is indistinguishable from a typed draft, so an
    // earlier version refused delivery forever on sessions that were in fact
    // idle and empty. These strings are verbatim `capture-pane -e -p` output.

    #[test]
    fn dim_placeholder_hint_is_empty_not_draft() {
        let cap = format!(
            "\u{1b}[2m⏺ earlier output\u{1b}[0m\n{}\n\u{1b}[39m❯\u{a0}\u{1b}[2mcheck the inbox again\u{1b}[0m\n{}\n  ADE main\n",
            RULE, RULE
        );
        assert_eq!(
            composer_state(&cap),
            ComposerState::Empty,
            "dim hint text must not be mistaken for the user's unsent input"
        );
    }

    #[test]
    fn undimmed_draft_alongside_ansi_is_still_a_draft() {
        // Same shape, but the content is NOT dim — that is real typed text.
        let cap = format!(
            "{}\n\u{1b}[39m❯\u{a0}commit the mail feature\u{1b}[0m\n{}\n  ADE main\n",
            RULE, RULE
        );
        assert_eq!(composer_state(&cap), ComposerState::Draft);
    }

    #[test]
    fn dim_reset_mid_line_counts_as_a_draft() {
        // A hint that is partly re-styled to normal intensity contains real
        // text; anything not provably dim must block delivery.
        let cap = format!(
            "{}\n\u{1b}[39m❯\u{a0}\u{1b}[2mhint\u{1b}[22m typed\u{1b}[0m\n{}\n",
            RULE, RULE
        );
        assert_eq!(composer_state(&cap), ComposerState::Draft);
    }

    #[test]
    fn dim_continuation_row_is_empty_but_normal_one_is_draft() {
        let ghost = format!(
            "{}\n\u{1b}[39m❯\u{a0}\n\u{1b}[2m  wrapped hint\u{1b}[0m\n{}\n",
            RULE, RULE
        );
        assert_eq!(composer_state(&ghost), ComposerState::Empty);
        let real = format!("{}\n❯\u{a0}\n  wrapped draft\n{}\n", RULE, RULE);
        assert_eq!(composer_state(&real), ComposerState::Draft);
    }

    #[test]
    fn sgr_stripping_keeps_frame_detection_working() {
        // Rules and prompt must still be found when the capture is full of
        // colour codes.
        let cap = format!(
            "\u{1b}[38;5;240m{}\u{1b}[0m\n\u{1b}[39m❯\u{a0}\u{1b}[0m\n\u{1b}[38;5;240m{}\u{1b}[0m\n",
            RULE, RULE
        );
        assert_eq!(composer_state(&cap), ComposerState::Empty);
    }
}

#[cfg(test)]
mod policy_tests {
    //! Deterministic tests for the delivery gate and the
    //! claim→send→archive/requeue machine, driven through a fake
    //! `DeliveryEnv`. No tmux, no real mailbox, no timing — every branch that
    //! decides whether text gets typed into somebody's terminal is pinned here.
    use super::*;
    use crate::claude_status::ClaudeState;
    use crate::tmux::{SendProgress, SendTextError};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const ADDR: &str = "4242:$7";
    const PANE: &str = "%9";

    struct FakeEnv {
        live: Option<(String, Vec<String>)>,
        composer: ComposerState,
        send: RefCell<Option<Result<(), SendTextError>>>,
        sent: RefCell<Vec<(String, String)>>,
    }

    impl FakeEnv {
        /// A world where everything is fine: one idle pane, empty composer.
        fn good() -> Self {
            FakeEnv {
                live: Some((ADDR.to_string(), vec![PANE.to_string()])),
                composer: ComposerState::Empty,
                send: RefCell::new(Some(Ok(()))),
                sent: RefCell::new(Vec::new()),
            }
        }
        fn with_send_err(mut self, msg: &str, progress: SendProgress) -> Self {
            self.send = RefCell::new(Some(Err(SendTextError {
                message: msg.to_string(),
                progress,
            })));
            self
        }
    }

    impl DeliveryEnv for FakeEnv {
        fn live_session(&self, _name: &str) -> Option<(String, Vec<String>)> {
            self.live.clone()
        }
        fn composer(&self, _pane_id: &str) -> ComposerState {
            self.composer
        }
        fn send_text(&self, pane_id: &str, text: &str) -> Result<(), SendTextError> {
            self.sent
                .borrow_mut()
                .push((pane_id.to_string(), text.to_string()));
            self.send.borrow_mut().take().unwrap_or(Ok(()))
        }
    }

    /// A deliverable request: local, has mail, Claude present and idle.
    fn ok_req<'a>(name: &'a str) -> DeliveryRequest<'a> {
        DeliveryRequest {
            session_name: name,
            is_local: true,
            has_pending_mail: true,
            claude_present: true,
            claude: None,
        }
    }

    fn refusal(g: Gate) -> String {
        match g {
            Gate::Refuse(r) => r,
            Gate::Deliver { pane_id } => {
                panic!("expected a refusal, got Deliver{{{}}}", pane_id)
            }
        }
    }

    // ── the happy path ───────────────────────────────────────────────

    #[test]
    fn idle_single_pane_empty_composer_delivers_to_that_pane() {
        let g = gate(&FakeEnv::good(), &ok_req("s"), ADDR);
        assert_eq!(g, Gate::Deliver { pane_id: PANE.to_string() });
    }

    // ── gate matrix: every refusal reason ────────────────────────────

    #[test]
    fn remote_is_gated_exactly_like_local() {
        // Remote delivery used to be refused here outright. It is supported
        // now, and WHERE the recipient is travels with the env
        // (`app::Delivery`) rather than being a policy decision — so a remote
        // request must pass the same gate a local one does, no more and no
        // less. If this ever starts refusing again, delivery to another
        // machine has silently regressed.
        let mut r = ok_req("s");
        r.is_local = false;
        assert!(matches!(
            gate(&FakeEnv::good(), &r, ADDR),
            Gate::Deliver { .. }
        ));

        // ...and it is still subject to every other refusal.
        let mut busy = ok_req("s");
        busy.is_local = false;
        busy.claude = Some(ClaudeState::Working);
        assert!(matches!(gate(&FakeEnv::good(), &busy, ADDR), Gate::Refuse(_)));
    }

    #[test]
    fn no_pending_mail_is_refused() {
        let mut r = ok_req("s");
        r.has_pending_mail = false;
        assert!(refusal(gate(&FakeEnv::good(), &r, ADDR)).contains("no pending mail"));
    }

    #[test]
    fn missing_claude_is_refused() {
        let mut r = ok_req("s");
        r.claude_present = false;
        assert!(refusal(gate(&FakeEnv::good(), &r, ADDR)).contains("no live Claude"));
    }

    #[test]
    fn working_is_refused() {
        let mut r = ok_req("s");
        r.claude = Some(ClaudeState::Working);
        assert!(refusal(gate(&FakeEnv::good(), &r, ADDR)).contains("is working"));
    }

    #[test]
    fn awaiting_approval_is_refused() {
        let mut r = ok_req("s");
        r.claude = Some(ClaudeState::AwaitingApproval);
        assert!(refusal(gate(&FakeEnv::good(), &r, ADDR)).contains("permission prompt"));
    }

    #[test]
    fn defensive_some_idle_is_refused_as_unproven() {
        // The rollup never stores Some(Idle); if it ever did, "idle" would be
        // unproven rather than confirmed, so it must not deliver.
        let mut r = ok_req("s");
        r.claude = Some(ClaudeState::Idle);
        assert!(refusal(gate(&FakeEnv::good(), &r, ADDR)).contains("isn't confirmed idle"));
    }

    #[test]
    fn unreadable_session_is_refused() {
        let mut env = FakeEnv::good();
        env.live = None;
        assert!(refusal(gate(&env, &ok_req("s"), ADDR)).contains("could not inspect panes"));
    }

    #[test]
    fn multi_pane_session_is_refused() {
        let mut env = FakeEnv::good();
        env.live = Some((ADDR.to_string(), vec!["%1".into(), "%2".into()]));
        assert!(refusal(gate(&env, &ok_req("s"), ADDR)).contains("has 2 panes"));
    }

    #[test]
    fn zero_pane_session_is_refused() {
        let mut env = FakeEnv::good();
        env.live = Some((ADDR.to_string(), vec![]));
        assert!(refusal(gate(&env, &ok_req("s"), ADDR)).contains("has 0 panes"));
    }

    #[test]
    fn address_mismatch_is_refused() {
        // Same session NAME, different live address: killed and recreated, or
        // recreated after a tmux server restart.
        let r = refusal(gate(&FakeEnv::good(), &ok_req("s"), "4242:$3"));
        assert!(r.contains("isn't the session this message was addressed to"));
        let r2 = refusal(gate(&FakeEnv::good(), &ok_req("s"), "9999:$7"));
        assert!(r2.contains("isn't the session this message was addressed to"));
    }

    #[test]
    fn empty_recorded_address_is_refused() {
        assert!(refusal(gate(&FakeEnv::good(), &ok_req("s"), ""))
            .contains("isn't the session this message was addressed to"));
    }

    #[test]
    fn draft_in_composer_is_refused() {
        let mut env = FakeEnv::good();
        env.composer = ComposerState::Draft;
        assert!(refusal(gate(&env, &ok_req("s"), ADDR)).contains("unsent text"));
    }

    #[test]
    fn workspace_trust_refusal_explains_the_fix() {
        let mut env = FakeEnv::good();
        env.composer = ComposerState::AwaitingTrust;
        let r = refusal(gate(&env, &ok_req("s"), ADDR));
        assert!(r.contains("workspace-trust"), "must name the cause: {}", r);
        assert!(r.contains("already-trusted directory"), "must give the fix: {}", r);
        assert!(
            r.contains("will not answer a security prompt"),
            "must state that ADE won't answer it: {}",
            r
        );
    }

    #[test]
    fn unknown_composer_is_refused() {
        let mut env = FakeEnv::good();
        env.composer = ComposerState::Unknown;
        assert!(refusal(gate(&env, &ok_req("s"), ADDR)).contains("could not confirm"));
    }

    #[test]
    fn cheap_checks_run_before_any_tmux_call() {
        // A request that fails a cheap in-memory check must be refused without
        // consulting the environment, so an env that would panic if touched
        // still yields a refusal. (This used to use a remote recipient as the
        // cheap refusal; remote is a legitimate target now, so it uses the
        // no-pending-mail check instead.)
        struct Exploding;
        impl DeliveryEnv for Exploding {
            fn live_session(&self, _: &str) -> Option<(String, Vec<String>)> {
                panic!("must not query tmux for an early refusal")
            }
            fn composer(&self, _: &str) -> ComposerState {
                panic!("must not capture a pane for an early refusal")
            }
            fn send_text(&self, _: &str, _: &str) -> Result<(), SendTextError> {
                panic!("must not send for an early refusal")
            }
        }
        let mut r = ok_req("s");
        r.has_pending_mail = false;
        assert!(matches!(gate(&Exploding, &r, ADDR), Gate::Refuse(_)));
    }

    // ── execute(): the claim → send → archive/requeue machine ────────

    fn tmp_base() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "ade-delivery-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Queue one message and claim it, returning (dir, claimed, msg).
    fn claimed_message() -> (PathBuf, Claimed, Message) {
        let dir = tmp_base();
        mail::write_message_in(&dir, "sender", "$1", "rcpt", "local", ADDR, "hello").unwrap();
        let (path, msg) = mail::read_inbox_in(&dir).into_iter().next().unwrap();
        let claimed = mail::claim(&dir, &path).unwrap();
        (dir, claimed, msg)
    }

    #[test]
    fn success_archives_and_sends_rendered_text_to_the_pane() {
        let (dir, claimed, msg) = claimed_message();
        let env = FakeEnv::good();
        assert_eq!(execute(&env, &dir, &claimed, &msg, PANE), Outcome::Delivered);
        // Archived, not left in inbox or claimed.
        assert!(dir.join("archive").join(format!("{}.json", claimed.id)).exists());
        assert!(mail::read_inbox_in(&dir).is_empty());
        assert!(!claimed.path.exists());
        // Exactly the prefixed line went to the exact pane.
        let sent = env.sent.borrow().clone();
        assert_eq!(sent, vec![(PANE.to_string(), "[ADE mail from sender] hello".to_string())]);
    }

    #[test]
    fn certain_failure_requeues_for_another_try() {
        let (dir, claimed, msg) = claimed_message();
        let env = FakeEnv::good().with_send_err("tmux refused", SendProgress::NotStarted);
        match execute(&env, &dir, &claimed, &msg, PANE) {
            Outcome::Requeued(e) => assert!(e.contains("tmux refused")),
            other => panic!("expected Requeued, got {:?}", other),
        }
        // Back in the inbox, nothing archived.
        assert_eq!(mail::read_inbox_in(&dir).len(), 1);
        assert!(!dir.join("archive").join(format!("{}.json", claimed.id)).exists());
    }

    #[test]
    fn ambiguous_failure_is_never_requeued() {
        // The body may already be typed, so retrying could double-submit. The
        // record stays claimed for stale-claim recovery to dead-letter.
        let (dir, claimed, msg) = claimed_message();
        let env = FakeEnv::good().with_send_err("enter failed", SendProgress::MayHaveTyped);
        match execute(&env, &dir, &claimed, &msg, PANE) {
            Outcome::Ambiguous(e) => assert!(e.contains("enter failed")),
            other => panic!("expected Ambiguous, got {:?}", other),
        }
        assert!(mail::read_inbox_in(&dir).is_empty(), "must NOT be requeued");
        assert!(claimed.path.exists(), "must stay claimed for recovery");
    }

    #[test]
    fn archive_failure_after_a_landed_send_does_not_requeue() {
        let (dir, claimed, msg) = claimed_message();
        // Block the archive move by parking a plain file where the dir goes.
        std::fs::write(dir.join("archive"), b"not a directory").unwrap();
        match execute(&FakeEnv::good(), &dir, &claimed, &msg, PANE) {
            Outcome::DeliveredButNotArchived(e) => assert!(!e.is_empty()),
            other => panic!("expected DeliveredButNotArchived, got {:?}", other),
        }
        // The text landed, so the message must not reappear in the inbox.
        assert!(mail::read_inbox_in(&dir).is_empty(), "must NOT be requeued");
    }

    #[test]
    fn requeue_failure_is_reported_with_both_causes() {
        let (dir, claimed, msg) = claimed_message();
        // Make the inbox un-creatable so the requeue move fails too.
        std::fs::remove_dir_all(dir.join("inbox")).unwrap();
        std::fs::write(dir.join("inbox"), b"not a directory").unwrap();
        let env = FakeEnv::good().with_send_err("send died", SendProgress::NotStarted);
        match execute(&env, &dir, &claimed, &msg, PANE) {
            Outcome::RequeueFailed { send, requeue } => {
                assert!(send.contains("send died"));
                assert!(!requeue.is_empty());
            }
            other => panic!("expected RequeueFailed, got {:?}", other),
        }
    }
}

#[cfg(test)]
mod held_signal_tests {
    //! The held signal exists so a stalled workstream is visible on the board
    //! rather than only in a router log nobody reads. These pin the two things
    //! that matter: the reason derived from cheap state, and whether it reads
    //! as "will clear itself" or "needs you".
    use super::*;
    use crate::claude_status::ClaudeState;
    use std::time::Duration;

    #[test]
    fn reason_is_derived_from_state_ade_already_has() {
        assert_eq!(
            HeldReason::from_session_state(true, Some(ClaudeState::Working)),
            HeldReason::Working
        );
        assert_eq!(
            HeldReason::from_session_state(true, Some(ClaudeState::AwaitingApproval)),
            HeldReason::AwaitingApproval
        );
        // Idle with a live Claude: the blocker needs a pane probe to name.
        assert_eq!(
            HeldReason::from_session_state(true, None),
            HeldReason::NeedsProbe
        );
    }

    #[test]
    fn no_claude_outranks_whatever_state_was_recorded() {
        // A stale status file must not make an empty session look merely busy —
        // nothing will ever pick that mail up.
        for st in [None, Some(ClaudeState::Working), Some(ClaudeState::AwaitingApproval)] {
            assert_eq!(
                HeldReason::from_session_state(false, st),
                HeldReason::NoClaude,
                "claude_present=false must dominate ({:?})",
                st
            );
        }
    }

    #[test]
    fn only_a_running_turn_counts_as_transient() {
        // Transient decides yellow vs red on the board: everything else is a
        // stall a human has to clear.
        assert!(HeldReason::Working.is_transient());
        for r in [
            HeldReason::AwaitingApproval,
            HeldReason::NoClaude,
            HeldReason::NeedsProbe,
        ] {
            assert!(!r.is_transient(), "{:?} should read as needing a human", r);
        }
    }

    #[test]
    fn every_reason_has_a_short_label() {
        for r in [
            HeldReason::Working,
            HeldReason::AwaitingApproval,
            HeldReason::NoClaude,
            HeldReason::NeedsProbe,
        ] {
            let l = r.short_label();
            assert!(!l.is_empty() && l.len() <= 10, "{:?} -> {:?}", r, l);
        }
    }

    #[test]
    fn wait_is_formatted_compactly_across_scales() {
        assert_eq!(format_wait(Duration::from_secs(0)), "0s");
        assert_eq!(format_wait(Duration::from_secs(59)), "59s");
        assert_eq!(format_wait(Duration::from_secs(60)), "1m");
        assert_eq!(format_wait(Duration::from_secs(3599)), "59m");
        assert_eq!(format_wait(Duration::from_secs(3600)), "1h");
        assert_eq!(format_wait(Duration::from_secs(86_399)), "23h");
        assert_eq!(format_wait(Duration::from_secs(86_400)), "1d");
        assert_eq!(format_wait(Duration::from_secs(86_400 * 9)), "9d");
    }

    #[test]
    fn held_threshold_outlasts_an_ordinary_turn() {
        // Must not fire for a session that is simply mid-response, or the
        // signal becomes noise and gets ignored.
        assert!(HELD_AFTER >= Duration::from_secs(60));
    }
}
