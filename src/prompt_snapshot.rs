//! Reads, sanitises, and parses the permission-prompt snapshots written by the
//! `awaiting_approval` hook (see `hooks/ade-claude-hook.sh`) into a structured
//! "ask" the HUD can show — the pending question plus its numbered options.
//!
//! **All snapshot content is untrusted.** It is whatever happened to be on a
//! Claude pane, which can include adversarial text, escape sequences, bidi
//! overrides, or absurd volume. So every line is stripped of control/ANSI/bidi/
//! zero-width characters and the output is bounded (line count + per-line
//! width). The parser is best-effort: when it can't confidently find a menu it
//! reports `PaneText`/`Unavailable` and hands back the sanitised raw lines
//! rather than guessing — the caller renders "open terminal", never a fabricated
//! menu. The `fingerprint` + `captured_unix` let a future actuation path detect
//! a stale/changed prompt and fail closed.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Keep at most this many lines from the tail of the captured pane.
const MAX_LINES: usize = 40;
/// Clamp each sanitised line to this many characters.
const MAX_LINE_WIDTH: usize = 200;
/// Never surface more than this many parsed options.
const MAX_OPTIONS: usize = 12;
/// Cap the reconstructed question length (characters).
const MAX_QUESTION: usize = 320;

/// How confidently we recovered the ask from the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AskSource {
    /// A numbered menu (1..=N) was parsed out of the pane.
    PaneMenu,
    /// A snapshot exists but no menu could be parsed — raw lines only.
    PaneText,
    /// No snapshot (session not awaiting, or file missing).
    Unavailable,
}

/// One selectable option from the permission menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AskOption {
    pub index: u32,
    pub label: String,
}

/// The structured (best-effort) view of what a session is asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ask {
    pub source: AskSource,
    /// The question text above the menu, if recovered.
    pub question: Option<String>,
    /// The numbered options, if a menu was parsed.
    pub options: Vec<AskOption>,
    /// Sanitised tail of the pane — the fallback the HUD can page when the
    /// structured parse is imperfect. Always safe to display.
    pub raw_lines: Vec<String>,
    /// Snapshot file mtime as unix seconds (age lets the HUD ignore stale asks).
    pub captured_unix: Option<u64>,
    /// Stable-within-run hash of the sanitised text (change/stale detection).
    pub fingerprint: Option<String>,
}

impl Ask {
    /// The "nothing to show" ask — no live prompt snapshot.
    pub fn unavailable() -> Ask {
        Ask {
            source: AskSource::Unavailable,
            question: None,
            options: Vec::new(),
            raw_lines: Vec::new(),
            captured_unix: None,
            fingerprint: None,
        }
    }
}

fn prompt_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cache").join("ade").join("claude-prompt"))
}

/// Read + parse the snapshot for a pane. Returns `Ask::unavailable()` when no
/// snapshot file exists (the common, non-awaiting case).
pub fn read(pane_id: &str) -> Ask {
    let Some(dir) = prompt_dir() else {
        return Ask::unavailable();
    };
    let path = dir.join(format!("{}.txt", pane_id));
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Ask::unavailable(),
    };
    let captured_unix = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    from_raw(&raw, captured_unix)
}

/// Parse an already-captured raw snapshot string (split out for testing).
pub fn from_raw(raw: &str, captured_unix: Option<u64>) -> Ask {
    let lines = sanitize(raw);
    if lines.is_empty() {
        return Ask::unavailable();
    }
    let fingerprint = Some(fingerprint(&lines));
    let (question, options, source) = parse(&lines);
    Ask {
        source,
        question,
        options,
        raw_lines: lines,
        captured_unix,
        fingerprint,
    }
}

/// Strip ANSI/control/bidi/zero-width from the capture and bound it to the last
/// `MAX_LINES` non-blank-bounded lines, each at most `MAX_LINE_WIDTH` wide.
pub fn sanitize(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = raw.split('\n').map(strip_line).collect();
    // Trim leading/trailing blank lines (the pane is usually bottom-anchored).
    while out.first().map(|l| l.is_empty()).unwrap_or(false) {
        out.remove(0);
    }
    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    if out.len() > MAX_LINES {
        out = out.split_off(out.len() - MAX_LINES); // keep the tail
    }
    out
}

/// Remove escape sequences, then filter one line down to safe printable chars
/// and clamp its width. Tabs become single spaces; trailing space is trimmed.
fn strip_line(line: &str) -> String {
    let no_ansi = strip_ansi(line);
    let mut out = String::new();
    let mut width = 0usize;
    for c in no_ansi.chars() {
        if width >= MAX_LINE_WIDTH {
            break;
        }
        if c == '\t' {
            out.push(' ');
            width += 1;
            continue;
        }
        let cp = c as u32;
        // C0 controls (except the tab handled above) and C1 controls.
        if cp < 0x20 || (0x80..=0x9f).contains(&cp) {
            continue;
        }
        // Zero-width joiners/spaces + BOM.
        if matches!(cp, 0x200b..=0x200d | 0xfeff) {
            continue;
        }
        // Bidi embeddings/overrides/isolates — can visually reorder text.
        if matches!(cp, 0x202a..=0x202e | 0x2066..=0x2069) {
            continue;
        }
        out.push(c);
        width += 1;
    }
    out.trim_end().to_string()
}

/// Drop CSI (`ESC [ … final`) and OSC (`ESC ] … BEL|ST`) sequences and lone
/// escapes. Defensive: the hook captures without `-e`, but a pane can still
/// print raw escapes itself.
fn strip_ansi(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '\u{1b}' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1; // consume ESC
        match chars.get(i) {
            Some('[') => {
                i += 1;
                while i < chars.len() && !matches!(chars[i], '\u{40}'..='\u{7e}') {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1; // consume the final byte
                }
            }
            Some(']') => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\u{07}' {
                        i += 1;
                        break;
                    }
                    if chars[i] == '\u{1b}' && chars.get(i + 1) == Some(&'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                // Lone ESC or a 2-char escape — drop one following char if any.
                if i < chars.len() {
                    i += 1;
                }
            }
        }
    }
    out
}

/// Best-effort menu extraction. Collects lines that look like `N. label`
/// (optionally caret-prefixed), keeps the run `1..=N`, and reconstructs the
/// question sitting just above the first option. Returns `PaneText` when no
/// clean menu is present.
fn parse(lines: &[String]) -> (Option<String>, Vec<AskOption>, AskSource) {
    // index -> (line position, label), first occurrence wins.
    let mut by_index: BTreeMap<u32, (usize, String)> = BTreeMap::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(opt) = parse_option_line(line) {
            by_index.entry(opt.index).or_insert((i, opt.label));
        }
    }

    let mut options = Vec::new();
    let mut first_line: Option<usize> = None;
    let mut expect = 1u32;
    while let Some((line_idx, label)) = by_index.get(&expect) {
        if first_line.is_none() {
            first_line = Some(*line_idx);
        }
        options.push(AskOption {
            index: expect,
            label: label.clone(),
        });
        if options.len() >= MAX_OPTIONS {
            break;
        }
        expect += 1;
    }

    match first_line {
        Some(fl) => {
            let question = question_above(lines, fl);
            (question, options, AskSource::PaneMenu)
        }
        None => (None, Vec::new(), AskSource::PaneText),
    }
}

/// Parse a single `N. label` / `N) label` option line, tolerating a leading
/// selection caret and indentation. Returns `None` for non-option lines
/// (including the description lines Claude renders under each option).
fn parse_option_line(line: &str) -> Option<AskOption> {
    let mut s = line.trim_start();
    for marker in ['❯', '>', '▶', '→', '●', '•', '*'] {
        if let Some(rest) = s.strip_prefix(marker) {
            s = rest.trim_start();
            break;
        }
    }
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let index: u32 = digits.parse().ok()?;
    if index == 0 || index > 99 {
        return None;
    }
    let rest = &s[digits.len()..];
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    let label = rest.trim();
    if label.is_empty() {
        return None;
    }
    Some(AskOption {
        index,
        label: truncate_chars(label, MAX_LINE_WIDTH),
    })
}

/// Reconstruct the question directly above the menu: skip blank/chrome lines,
/// then gather the contiguous block of text lines (up to 4) and join them.
fn question_above(lines: &[String], first_option: usize) -> Option<String> {
    if first_option == 0 {
        return None;
    }
    // Walk up to the first real text line above the menu.
    let mut i = first_option;
    loop {
        i -= 1;
        let l = &lines[i];
        if !l.trim().is_empty() && !is_chrome(l) {
            break;
        }
        if i == 0 {
            return None;
        }
    }
    // Gather this line and preceding contiguous text lines.
    let mut collected: Vec<&str> = Vec::new();
    let mut j = i as isize;
    while j >= 0 {
        let l = &lines[j as usize];
        if l.trim().is_empty() || is_chrome(l) {
            break;
        }
        collected.push(l.trim());
        if collected.len() >= 4 {
            break;
        }
        j -= 1;
    }
    collected.reverse();
    let q = collected.join(" ");
    let q = q.trim();
    if q.is_empty() {
        None
    } else {
        Some(truncate_chars(q, MAX_QUESTION))
    }
}

/// Box-drawing rules and prompt hint lines that are not part of the question.
fn is_chrome(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let boxish = t.chars().all(|c| {
        matches!(
            c,
            '─' | '│' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼'
                | '╭' | '╮' | '╰' | '╯' | '═' | '━' | '·' | '-' | '—' | ' '
        )
    });
    if boxish {
        return true;
    }
    let lower = t.to_lowercase();
    lower.contains("enter to select")
        || lower.contains("to navigate")
        || lower.contains("esc to")
        || lower.contains("↑/↓")
        || lower.contains("↑/↓ to")
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn fingerprint(lines: &[String]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    lines.len().hash(&mut h);
    for l in lines {
        l.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// Current unix seconds — small helper so callers can stamp non-file snapshots.
#[allow(dead_code)]
pub fn now_unix() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic Claude Code permission menu, matching the shape ADE captures.
    const SAMPLE: &str = "\
some earlier output line\n\
\n\
────────────────────────────────────────────\n\
 ☐ Close slot H\n\
\n\
The safety check reports \"unsafe\" only because of untracked pipeline\n\
scaffolding (.openclaw/, WORKSTREAM.md). How do you want to proceed?\n\
\n\
❯ 1. Force close (Recommended)\n\
     Run close --force. Releases slot H, removes the registry entry.\n\
  2. Abort\n\
     Don't close. Leave the workstream open.\n\
  3. Type something.\n\
────────────────────────────────────────────\n\
  4. Chat about this\n\
\n\
 Enter to select · ↑/↓ to navigate · Esc to cancel\n";

    #[test]
    fn parses_question_and_all_four_options() {
        let ask = from_raw(SAMPLE, Some(1_700_000_000));
        assert_eq!(ask.source, AskSource::PaneMenu);
        assert_eq!(ask.options.len(), 4);
        assert_eq!(ask.options[0].index, 1);
        assert_eq!(ask.options[0].label, "Force close (Recommended)");
        assert_eq!(ask.options[1].label, "Abort");
        assert_eq!(ask.options[3].index, 4);
        assert_eq!(ask.options[3].label, "Chat about this");
        let q = ask.question.expect("question recovered");
        assert!(q.contains("How do you want to proceed?"), "got: {q}");
        // The checkbox chrome / box rules must not bleed into the question.
        assert!(!q.contains("☐"));
        assert!(!q.contains("─"));
        assert_eq!(ask.captured_unix, Some(1_700_000_000));
        assert!(ask.fingerprint.is_some());
    }

    #[test]
    fn caret_and_indentation_are_tolerated() {
        let opt = parse_option_line("❯ 1. Force close (Recommended)").unwrap();
        assert_eq!(opt.index, 1);
        assert_eq!(opt.label, "Force close (Recommended)");
        let opt2 = parse_option_line("  2. Abort").unwrap();
        assert_eq!(opt2.index, 2);
        // Description lines under an option are NOT options.
        assert!(parse_option_line("     Run close --force. Releases slot H.").is_none());
        assert!(parse_option_line("plain prose, no number").is_none());
    }

    #[test]
    fn no_menu_falls_back_to_pane_text_never_a_guess() {
        let ask = from_raw("just some running output\nno menu here\n", None);
        assert_eq!(ask.source, AskSource::PaneText);
        assert!(ask.options.is_empty());
        assert!(ask.question.is_none());
        assert!(!ask.raw_lines.is_empty()); // fallback is still shown
    }

    #[test]
    fn missing_option_one_is_not_a_menu() {
        // Numbers that don't start at 1 aren't a permission menu.
        let ask = from_raw("2. two\n3. three\n", None);
        assert_eq!(ask.source, AskSource::PaneText);
    }

    #[test]
    fn strips_ansi_control_and_bidi() {
        let dirty = "\x1b[31m 1.\x1b[0m Fo\u{200b}rce \x07close\u{202e}evil";
        let cleaned = strip_line(dirty);
        assert!(!cleaned.contains('\u{1b}'));
        assert!(!cleaned.contains('\u{200b}'));
        assert!(!cleaned.contains('\u{202e}'));
        assert!(!cleaned.contains('\u{07}'));
        assert!(cleaned.contains("Force close"), "got: {cleaned:?}");
    }

    #[test]
    fn line_width_is_bounded() {
        let long = "x".repeat(1000);
        assert!(strip_line(&long).chars().count() <= MAX_LINE_WIDTH);
    }

    #[test]
    fn empty_capture_is_unavailable() {
        assert_eq!(from_raw("\n\n\n", None).source, AskSource::Unavailable);
    }
}
